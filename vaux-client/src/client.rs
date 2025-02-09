use std::{
    collections::HashMap,
    io::{Read, Write},
    net::TcpStream,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
    vec,
};

use bytes::BytesMut;
use vaux_mqtt::{
    decode, encode, property::Property, ConnAck, Connect, Packet, PropertyType, PubResp, QoSLevel,
    Reason, Subscribe, Subscription,
};

use crate::{ErrorKind, MqttConnection, MqttError};

const DEFAULT_RECV_MAX: u16 = 100;
const DEFAULT_SESSION_EXPIRY: u32 = 1000;
// 64K is the default max packet size
const DEFAULT_MAX_PACKET_SIZE: usize = 64 * 1024;
const MAX_QUEUE_LEN: usize = 100;

#[derive(Debug)]
struct MqttStream<'a> {
    tcp: Option<TcpStream>,
    tls: Option<rustls::Stream<'a, rustls::ClientConnection, TcpStream>>,
}

impl<'a> MqttStream<'a> {
    fn new_tcp(tcp: TcpStream) -> Self {
        Self {
            tcp: Some(tcp),
            tls: None,
        }
    }

    fn new_tls(tls_conn: &'a mut rustls::ClientConnection, tcp: &'a mut TcpStream) -> Self {
        Self {
            tcp: None,
            tls: Some(rustls::Stream::new(tls_conn, tcp)),
        }
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
        if let Some(ref mut tcp) = self.tcp {
            return tcp.set_read_timeout(timeout);
        }
        if let Some(ref mut tls) = self.tls {
            return tls.sock.set_read_timeout(timeout);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "no stream available",
        ))
    }

    fn shutdown(&mut self) -> std::io::Result<()> {
        if let Some(ref mut tcp) = self.tcp {
            return tcp.shutdown(std::net::Shutdown::Both);
        }
        if let Some(ref mut tls) = self.tls {
            return tls.sock.shutdown(std::net::Shutdown::Both);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "no stream available",
        ))
    }
}

impl<'a> Read for MqttStream<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(ref mut tcp) = self.tcp {
            return tcp.read(buf);
        }
        if let Some(ref mut tls) = self.tls {
            return tls.read(buf);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "no stream available",
        ))
    }
}

impl<'a> Write for MqttStream<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(ref mut tcp) = self.tcp {
            return tcp.write(buf);
        }
        if let Some(ref mut tls) = self.tls {
            return tls.write(buf);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "no stream available",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(ref mut tcp) = self.tcp {
            return tcp.flush();
        }
        if let Some(ref mut tls) = self.tls {
            return tls.flush();
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "no stream available",
        ))
    }
}

#[derive(Debug)]
pub struct MqttClient {
    auto_ack: bool,
    auto_packet_id: bool,
    last_packet_id: u16,
    receive_max: u16,
    connected: Arc<Mutex<bool>>,
    last_error: Arc<Mutex<Option<MqttError>>>,
    session_expiry: u32,
    client_id: Arc<Mutex<Option<String>>>,
    producer: crossbeam_channel::Sender<vaux_mqtt::Packet>,
    consumer: crossbeam_channel::Receiver<vaux_mqtt::Packet>,
    packet_send: Option<crossbeam_channel::Receiver<vaux_mqtt::Packet>>,
    packet_recv: Option<crossbeam_channel::Sender<vaux_mqtt::Packet>>,
    subscriptions: Vec<Subscription>,
    pending_qos1: Arc<Mutex<Vec<Packet>>>,
    max_packet_size: usize,
}

impl Default for MqttClient {
    fn default() -> Self {
        Self::new(
            &uuid::Uuid::new_v4().to_string(),
            true,
            DEFAULT_RECV_MAX,
            true,
        )
    }
}

impl MqttClient {
    /// Creates a new MQTT client with the specified host, port, client ID, and
    /// auto ack settings. The client ID is required and must be unique for the
    /// broker. If the client ID is not specified, a UUID will be generated and
    /// used as the client ID.
    pub fn new(client_id: &str, auto_ack: bool, receive_max: u16, auto_packet_id: bool) -> Self {
        let (producer, packet_send): (
            crossbeam_channel::Sender<vaux_mqtt::Packet>,
            crossbeam_channel::Receiver<vaux_mqtt::Packet>,
        ) = crossbeam_channel::unbounded();
        let (packet_recv, consumer): (
            crossbeam_channel::Sender<vaux_mqtt::Packet>,
            crossbeam_channel::Receiver<vaux_mqtt::Packet>,
        ) = crossbeam_channel::unbounded();
        Self {
            auto_ack,
            auto_packet_id,
            last_packet_id: 0,
            last_error: Arc::new(Mutex::new(None)),
            receive_max,
            connected: Arc::new(Mutex::new(false)),
            session_expiry: DEFAULT_SESSION_EXPIRY,
            client_id: Arc::new(Mutex::new(Some(client_id.to_string()))),
            producer,
            consumer,
            packet_send: Some(packet_send),
            packet_recv: Some(packet_recv),
            subscriptions: Vec::new(),
            pending_qos1: Arc::new(Mutex::new(Vec::new())),
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
        }
    }

    /// Gets a new message producer channel. This channel is used to send MQTT packets
    /// to the remote broker. The producer channel is cloned and returned so that
    /// multiple threads can send messages to the remote broker.
    pub fn producer(&self) -> crossbeam_channel::Sender<vaux_mqtt::Packet> {
        self.producer.clone()
    }

    /// Gets a new message consumer channel. This channel is used to receive MQTT packets
    /// from the remote broker. The consumer channel is cloned and returned so that
    /// multiple threads can receive messages from the remote broker. Consumers do not
    /// get duplicate messages using this method. A consumer will only receive a message
    /// once and no 2 consumers will receive the same message.
    pub fn consumer(&mut self) -> crossbeam_channel::Receiver<vaux_mqtt::Packet> {
        self.consumer.clone()
    }

    pub fn max_packet_size(&self) -> usize {
        self.max_packet_size
    }

    pub fn set_max_packet_size(&mut self, max_packet_size: usize) {
        self.max_packet_size = max_packet_size;
    }

    pub fn connected(&self) -> bool {
        *self.connected.lock().unwrap()
    }

    pub fn session_expiry(&self) -> u32 {
        self.session_expiry
    }

    /// Sets the session expiry for the client. The session expiry is the number
    /// of seconds that the broker will maintain the session for the client after
    /// the client disconnects. If the client reconnects within the session expiry
    /// interval, the broker will resume the session. If the client does not
    /// reconnect within the session expiry interval, the broker will discard the
    /// session and any state associated with the session. The session_expiry must
    /// be set prior to calling connect for the value to be used.
    ///
    /// The default session expiry is 0 seconds, so no session information would be
    /// stored by the broker with the default set.
    /// Example:
    /// ```
    /// use vaux_client::MqttClient;
    ///
    /// let mut client = MqttClient::default();
    /// // set the session expiry to 1 day
    /// client.set_session_expiry(60 * 60 * 24);
    /// ```
    pub fn set_session_expiry(&mut self, session_expiry: u32) {
        self.session_expiry = session_expiry;
    }

    /// Helper method to subscribe to the topics in the topic filter. This helper
    /// subscribes with a QoS level of "At Most Once", or 0. A SUBACK will
    /// typically be returned on the consumer on a successful subscribe.
    pub fn subscribe(
        &mut self,
        packet_id: u16,
        topic_filter: &[&str],
        qos: QoSLevel,
    ) -> std::result::Result<(), Box<crossbeam_channel::SendError<Packet>>> {
        let mut subscribe = Subscribe::default();
        subscribe.set_packet_id(packet_id);
        for topic in topic_filter {
            let subscription = Subscription {
                filter: (*topic).to_string(),
                qos,
                ..Default::default()
            };
            self.subscriptions.push(subscription.clone());
            subscribe.add_subscription(subscription);
        }
        self.producer
            .send(vaux_mqtt::Packet::Subscribe(subscribe))
            .map_err(|e| e.into())
    }

    /// Attempts to start an MQTT session with the remote broker. The client will
    /// attempt to connect to the remote broker and send a CONNECT packet. If the
    /// client is unable to connect to the remote broker, an error will be returned.
    /// The ```max_wait``` parameter is used to determine how long the client will
    /// wait for the connection to be established. If the connection is not established
    /// within the ```max_wait``` interval, an error will be returned.
    /// Example:
    /// ```
    /// use vaux_client::MqttClient;
    /// use vaux_client::MqttConnection;
    /// use std::time::Duration;
    ///
    /// let mut client = MqttClient::default();
    /// let connection: MqttConnection;
    ///
    /// match MqttConnection::new().with_host("localhost").with_port(1883).connect() {
    ///     Ok(c) => {
    ///         connection = c;
    ///     }
    ///     Err(e) => {
    ///         println!("unable to establish TCP connection: {:?}", e);
    ///        return;
    ///     }
    /// }
    /// let handle: Option<std::thread::JoinHandle<_>>;
    /// match client.try_start(Duration::from_millis(5000), connection, true) {
    ///    Ok(h) => {
    ///       handle = Some(h);
    ///       println!("connected to broker");
    ///   }
    ///
    ///  Err(e) => {
    ///    println!("unable to connect to broker: {:?}", e);
    ///   }
    /// }
    /// ```
    ///
    pub fn try_start(
        &mut self,
        max_wait: Duration,
        connection: MqttConnection,
        clean_start: bool,
    ) -> crate::Result<JoinHandle<crate::Result<()>>> {
        let handle = self.start(connection, clean_start);
        let start = std::time::Instant::now();
        while !self.connected() {
            let last_error = self.last_error.lock();
            if let Ok(last_error) = last_error {
                if let Some(last_error) = last_error.as_ref() {
                    match handle.join() {
                        Ok(result) => {
                            result?;
                        }
                        Err(e) => {
                            return Err(MqttError::new(
                                &format!("unable to join thread: {:?}", e),
                                ErrorKind::Transport,
                            ));
                        }
                    }
                    return Err(last_error.clone());
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            if start.elapsed() > max_wait {
                return Err(MqttError::new(
                    "timeout waiting for connection",
                    ErrorKind::Timeout,
                ));
            }
        }
        Ok(handle)
    }

    /// Starts the MQTT client thread. The MQTT client thread will send packets
    /// to the remote broker that it receives on the producer channel and make
    /// packets available on the consumer channel that it receives from the broker
    ///
    /// The MQTT client thread can be stopped by calling the stop method or by
    /// sending a DISCONNECT packet on the producer channel.
    ///
    /// There are cases where the client may not be able to send a message (e.g.
    /// QoS 1 and no more messages can be sent). In these cases, the message will
    /// be queued and sent when the client is able to send it or until the maximum
    /// queue size is reached based on packet size and/or count. The client will
    /// thread will terminate if the queue is full and the client is unable to send.
    ///
    /// Queued messages will be sent in the order they were received. Any messages
    /// that are queued when the client is stopped will remain queued until the client
    /// is started again or the client is dropped.
    pub fn start(
        &mut self,
        mut connection: MqttConnection,
        clean_start: bool,
    ) -> JoinHandle<crate::Result<()>> {
        let packet_recv = self.packet_recv.as_ref().unwrap().clone();
        let packet_send = self.packet_send.as_ref().unwrap().clone();
        let auto_ack = self.auto_ack;
        let receive_max = self.receive_max;
        let pending_qos1 = self.pending_qos1.clone();
        let mut last_packet_id = self.last_packet_id;
        let auto_packet_id = self.auto_packet_id;
        let max_packet_size = self.max_packet_size;
        let client_id = self.client_id.clone();
        let session_expiry = self.session_expiry;
        let connected = self.connected.clone();
        let credentials = connection.credentials();
        let last_error = self.last_error.clone();

        thread::spawn(move || {
            let mut buffer = vec![0; max_packet_size];
            let mut offset = 0;

            let mut stream = if connection.tls {
                MqttStream::new_tls(
                    connection.tls_conn.as_mut().unwrap(),
                    connection.tcp_socket.as_mut().unwrap(),
                )
            } else {
                MqttStream::new_tcp(connection.tcp_socket.take().unwrap())
            };

            if let Err(e) = stream.set_read_timeout(Some(Duration::from_millis(100))) {
                return Err(MqttError::new(
                    &format!("unable to set read timeout: {}", e),
                    ErrorKind::Transport,
                ));
            }

            match Self::send_connect(
                &mut stream,
                credentials,
                client_id,
                session_expiry,
                clean_start,
                connected,
                &mut buffer,
                &mut offset,
            ) {
                Ok(_) => {}
                Err(e) => {
                    let last_error = last_error.lock();
                    if let Ok(mut last_error) = last_error {
                        *last_error = Some(e.clone());
                    }
                    stream.shutdown().unwrap();
                    return Err(e);
                }
            }
            let mut pending_recv_ack: HashMap<u16, Packet> = HashMap::new();
            let mut pending_publish: Vec<Packet> = Vec::new();
            // TODO add size tracking to pending publish
            // let mut pending_publish_size = 0;
            let mut qos_1_remaining = receive_max;
            pending_publish.append(&mut pending_qos1.lock().unwrap());
            loop {
                match MqttClient::read_next(&mut stream, max_packet_size, &mut buffer, &mut offset)
                {
                    Ok(result) => {
                        if let Some(p) = result {
                            match &p {
                                Packet::Disconnect(d) => {
                                    // TODO handle disconnect - verify shutdown behavior
                                    stream.shutdown().unwrap();
                                    pending_qos1.lock().unwrap().append(&mut pending_publish);
                                    return Err(MqttError::new(
                                        &format!("disconnect received: {:?}", d),
                                        ErrorKind::Protocol(d.reason),
                                    ));
                                }
                                Packet::Publish(publish) => {
                                    match publish.qos() {
                                        vaux_mqtt::QoSLevel::AtMostOnce => {}
                                        vaux_mqtt::QoSLevel::AtLeastOnce => {
                                            if auto_ack {
                                                let mut puback = PubResp::new_puback();
                                                if let Some(packet_id) = publish.packet_id {
                                                    puback.packet_id = packet_id;
                                                } else {
                                                    stream.shutdown().unwrap();
                                                    return Err(MqttError::new(
                                                        "protocol error, no packet ID with QAS > 0",
                                                        ErrorKind::Protocol(
                                                            Reason::MalformedPacket,
                                                        ),
                                                    ));
                                                }
                                                if MqttClient::send(
                                                    &mut stream,
                                                    Packet::PubAck(puback),
                                                )
                                                .is_err()
                                                {
                                                    // TODO handle the pub ack next time through
                                                    // push a message to the last error channel
                                                    eprintln!("unable to send puback");
                                                }
                                            }
                                        }
                                        vaux_mqtt::QoSLevel::ExactlyOnce => todo!(),
                                    }
                                }
                                Packet::PubAck(puback) => {
                                    if let Some(_p) = pending_recv_ack.remove(&puback.packet_id) {
                                        if qos_1_remaining < receive_max {
                                            qos_1_remaining += 1;
                                        }
                                    } else {
                                        // TODO PUBACK that was not expected
                                    }
                                }
                                _ => {}
                            }
                            if let Err(e) = packet_recv.send(p.clone()) {
                                stream.shutdown().unwrap();
                                pending_qos1.lock().unwrap().append(&mut pending_publish);
                                return Err(MqttError::new(
                                    &format!("unable to send packet to consumer: {}", e),
                                    ErrorKind::Transport,
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        if e.kind() != ErrorKind::Timeout {
                            // there may be nothing to read so this is not necessarily an error
                            // TODO configure for disconnect/reconnect, PING or stop on timeouts
                        }
                    }
                };
                if let Ok(mut packet) = packet_send.recv_timeout(Duration::from_millis(10)) {
                    if let Packet::Publish(mut p) = packet.clone() {
                        if p.qos() == QoSLevel::AtLeastOnce {
                            if auto_packet_id {
                                last_packet_id += 1;
                                p.packet_id = Some(last_packet_id);
                                pending_recv_ack.insert(last_packet_id, Packet::Publish(p.clone()));
                            } else if let Some(packet_id) = p.packet_id {
                                pending_recv_ack.insert(packet_id, Packet::Publish(p.clone()));
                            } else {
                                // TODO handle error
                                eprintln!("no packet id");
                            }
                            if qos_1_remaining > 0 {
                                qos_1_remaining -= 1;
                                packet = Packet::Publish(p);
                            } else {
                                // TODO cannot send the packet - need to inform client
                                if pending_publish.len() < MAX_QUEUE_LEN {
                                    // && pending_publish_size < MAX_QUEUE_SIZE {
                                    pending_publish.push(Packet::Publish(p));
                                    continue;
                                }
                            }
                        }
                    } else if let Packet::Disconnect(_d) = packet.clone() {
                        if let Err(e) = MqttClient::send(&mut stream, packet) {
                            eprintln!("ERROR sending packet to remote: {}", e.message());
                        }
                        stream.shutdown().unwrap();
                        pending_qos1.lock().unwrap().append(&mut pending_publish);
                        return Ok(());
                    }
                    if let Err(e) = MqttClient::send(&mut stream, packet) {
                        eprintln!("ERROR sending packet to remote: {}", e.message());
                    }
                    // send any pending QOS-1 publish packets that we are able to send
                    while !pending_publish.is_empty() && qos_1_remaining > 0 {
                        while !pending_publish.is_empty() && qos_1_remaining > 0 {
                            let packet = pending_publish.remove(0);
                            // pending_publish_size -= packet.encoded_size();
                            if let Err(e) = MqttClient::send(&mut stream, packet.clone()) {
                                pending_publish.insert(0, packet);
                                // TODO notify calling client of error
                                eprintln!("ERROR sending packet to remote: {}", e.message());
                            } else {
                                qos_1_remaining += 1;
                            }
                        }
                    }
                }
            }
        })
    }

    pub fn stop(&mut self) {
        let disconnect = Packet::Disconnect(Default::default());
        if let Err(e) = self.producer.send(disconnect) {
            eprintln!("unable to send disconnect: {}", e);
        }
    }

    fn send_connect(
        stream: &mut MqttStream,
        credentials: Option<(String, String)>,
        client_id: Arc<Mutex<Option<String>>>,
        session_expiry: u32,
        clean_start: bool,
        connected: Arc<Mutex<bool>>,
        buffer: &mut Vec<u8>,
        offset: &mut usize,
    ) -> crate::Result<ConnAck> {
        let mut connect = Connect::default();
        connect.clean_start = clean_start;
        // scoped mutex guard to set the connect packet client id
        {
            let set_id = client_id.lock().unwrap();
            if set_id.is_some() {
                connect.client_id = (*set_id.as_ref().unwrap()).to_string();
            }
        }
        connect
            .properties_mut()
            .set_property(Property::SessionExpiryInterval(session_expiry));
        if let Some((username, password)) = credentials {
            connect.username = Some(username);
            connect.password = Some(password.into_bytes());
        }
        let connect_packet = Packet::Connect(Box::new(connect));
        // let mut buffer = [0u8; 128];
        let mut dest = BytesMut::default();
        let result = encode(connect_packet, &mut dest);
        if let Err(e) = result {
            panic!("Failed to encode packet: {:?}", e);
        }
        match stream.write_all(&dest) {
            Ok(_) => {
                match MqttClient::read_next(stream, DEFAULT_MAX_PACKET_SIZE, buffer, offset) {
                    Ok(Some(packet)) => match packet {
                        Packet::ConnAck(connack) => {
                            Self::handle_connack(connack, connected, client_id)
                        }
                        Packet::Disconnect(_disconnect) => {
                            // TODO return the disconnect reason as MQTT error
                            panic!("disconnect");
                        }
                        _ => Err(MqttError::new(
                            "unexpected packet type",
                            ErrorKind::Protocol(Reason::ProtocolErr),
                        )),
                    },
                    Ok(None) => Err(MqttError::new(
                        "no MQTT packet received",
                        ErrorKind::Protocol(Reason::ProtocolErr),
                    )),
                    Err(e) => Err(MqttError::new(
                        &format!("unable to read stream: {}", e),
                        ErrorKind::Transport,
                    )),
                }
            }

            //     Ok(len) => match decode(&mut BytesMut::from(&buffer[0..len])) {
            //         Ok(data_read) => {
            //             if let Some((packet, _decode_len)) = data_read {
            //                 match packet {
            //                     Packet::ConnAck(connack) => {
            //                         Self::handle_connack(connack, connected, client_id)
            //                     }
            //                     Packet::Disconnect(_disconnect) => {
            //                         // TODO return the disconnect reason as MQTT error
            //                         panic!("disconnect");
            //                     }
            //                     _ => Err(MqttError::new(
            //                         "unexpected packet type",
            //                         ErrorKind::Protocol(Reason::ProtocolErr),
            //                     )),
            //                 }
            //             } else {
            //                 Err(MqttError::new(
            //                     "no MQTT packet received",
            //                     ErrorKind::Protocol(Reason::ProtocolErr),
            //                 ))
            //             }
            //         }
            //         Err(e) => Err(MqttError::new(&e.to_string(), ErrorKind::Codec)),
            //     },
            //     Err(e) => Err(MqttError::new(
            //         &format!("unable to read stream: {}", e),
            //         ErrorKind::Transport,
            //     )),
            // },
            Err(e) => Err(MqttError::new(
                &format!("Unable to write packet(s) to broker: {}", e),
                ErrorKind::Transport,
            )),
        }
    }

    fn handle_connack(
        connack: ConnAck,
        connected: Arc<Mutex<bool>>,
        client_id: Arc<Mutex<Option<String>>>,
    ) -> crate::Result<ConnAck> {
        let set_id = client_id.lock().unwrap();
        let client_id_set = set_id.is_some();
        if connack.reason() != Reason::Success {
            // TODO return the connack reason as MQTT error with reason code
            let mut connected = connected.lock().unwrap();
            *connected = false;
            return Err(MqttError::new(
                "connection refused",
                ErrorKind::Protocol(connack.reason()),
            ));
        } else {
            let mut connected = connected.lock().unwrap();
            *connected = true;
        }
        if !client_id_set {
            match connack
                .properties()
                .get_property(&PropertyType::AssignedClientId)
            {
                Some(Property::AssignedClientId(id)) => {
                    let mut client_id = client_id.lock().unwrap();
                    *client_id = Some(id.to_owned());
                }
                _ => {
                    // handle error here for required property
                    Err(MqttError::new(
                        "no assigned client id",
                        ErrorKind::Protocol(Reason::InvalidClientId),
                    ))?;
                }
            }
        }
        // TODO set server properties based on ConnAck
        Ok(connack)
    }

    fn read_next(
        connection: &mut dyn std::io::Read,
        max_packet_size: usize,
        buffer: &mut Vec<u8>,
        offset: &mut usize,
    ) -> crate::Result<Option<Packet>> {
        let mut bytes_read = *offset;
        loop {
            if bytes_read > 0 {
                let bytes_mut = &mut BytesMut::from(&buffer[0..bytes_read]);
                match decode(bytes_mut) {
                    Ok(data_read) => {
                        if let Some((packet, decode_len)) = data_read {
                            if decode_len < bytes_read as u32 {
                                buffer.copy_within(decode_len as usize..bytes_read, 0);
                                // adjust offset to end of decoded bytes
                                *offset = bytes_read - decode_len as usize;
                            } else {
                                *offset = 0;
                            }
                            return Ok(Some(packet));
                        } else {
                            return Ok(None);
                        }
                    }
                    Err(e) => match e.kind {
                        vaux_mqtt::codec::ErrorKind::InsufficientData(_expected, _actual) => {
                            // fall through the the socket read
                        }
                        _ => {
                            return Err(MqttError::new(
                                &e.to_string(),
                                crate::ErrorKind::Protocol(Reason::ProtocolErr),
                            ));
                        }
                    },
                }
            }
            match connection.read(&mut buffer[*offset..max_packet_size]) {
                Ok(len) => {
                    if len == 0 && bytes_read == 0 {
                        return Ok(None);
                    }
                    bytes_read += len;
                    *offset = bytes_read;
                }
                Err(e) => match e.kind() {
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                        return Err(MqttError::new(&e.to_string(), ErrorKind::Timeout));
                    }
                    _ => return Err(MqttError::new(&e.to_string(), ErrorKind::IO)),
                },
            }
        }
    }

    pub fn send(
        connection: &mut dyn std::io::Write,
        packet: Packet,
    ) -> crate::Result<Option<Packet>> {
        let mut dest = BytesMut::default();
        let result = encode(packet, &mut dest);
        if let Err(e) = result {
            panic!("Failed to encode packet: {:?}", e);
        }
        if let Err(e) = connection.write_all(&dest) {
            eprintln!("unexpected send error {:#?}", e);
            // TODO higher fidelity error handling
            return Err(MqttError::new(
                &format!("unable to send packet: {}", e),
                ErrorKind::IO,
            ));
        }
        Ok(None)
    }
}
