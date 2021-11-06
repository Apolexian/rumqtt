use crate::{Event, Incoming, Outgoing, Request};

use bytes::BytesMut;
use mqttbytes::v4::*;
use mqttbytes::*;
use std::collections::VecDeque;
use std::{io, mem, time::Instant};

/// Errors during state handling
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Io Error while state is passed to network
    #[error("Io error {0:?}")]
    Io(#[from] io::Error),
    /// Broker's error reply to client's connect packet
    #[error("Connect return code `{0:?}`")]
    Connect(ConnectReturnCode),
    /// Invalid state for a given operation
    #[error("Invalid state for a given operation")]
    InvalidState,
    /// Received a packet (ack) which isn't asked for
    #[error("Received unsolicited ack pkid {0}")]
    Unsolicited(u16),
    /// Last pingreq isn't acked
    #[error("Last pingreq isn't acked")]
    AwaitPingResp,
    /// Received a wrong packet while waiting for another packet
    #[error("Received a wrong packet while waiting for another packet")]
    WrongPacket,
    #[error("Timeout while waiting to resolve collision")]
    CollisionTimeout,
    #[error("Mqtt serialization/deserialization error")]
    Deserialization(mqttbytes::Error),
}

impl From<mqttbytes::Error> for StateError {
    fn from(e: mqttbytes::Error) -> StateError {
        StateError::Deserialization(e)
    }
}

/// State of the mqtt connection.
// Design: Methods will just modify the state of the object without doing any network operations
// Design: All inflight queues are maintained in a pre initialized vec with index as packet id.
// This is done for 2 reasons
// Bad acks or out of order acks aren't O(n) causing cpu spikes
// Any missing acks from the broker are detected during the next recycled use of packet ids
#[derive(Debug, Clone)]
pub struct MqttState {
    /// Status of last ping
    pub await_pingresp: bool,
    /// Collision ping count. Collisions stop user requests
    /// which inturn trigger pings. Multiple pings without
    /// resolving collisions will result in error
    pub collision_ping_count: usize,
    /// Last incoming packet time
    last_incoming: Instant,
    /// Last outgoing packet time
    last_outgoing: Instant,
    /// Packet id of the last outgoing packet
    pub(crate) last_pkid: u16,
    /// Number of outgoing inflight publishes
    pub(crate) inflight: u16,
    /// Maximum number of allowed inflight
    pub(crate) max_inflight: u16,
    /// Outgoing QoS 1, 2 publishes which aren't acked yet
    pub(crate) outgoing_pub: Vec<Option<Publish>>,
    /// Packet ids of released QoS 2 publishes
    pub(crate) outgoing_rel: Vec<Option<u16>>,
    /// Packet ids on incoming QoS 2 publishes
    pub(crate) incoming_pub: Vec<Option<u16>>,
    /// Last collision due to broker not acking in order
    pub collision: Option<Publish>,
    /// Buffered incoming packets
    pub events: VecDeque<Event>,
    /// Write buffer
    pub write: BytesMut,
}

impl MqttState {
    /// Creates new mqtt state. Same state should be used during a
    /// connection for persistent sessions while new state should
    /// instantiated for clean sessions
    pub fn new(max_inflight: u16) -> Self {
        MqttState {
            await_pingresp: false,
            collision_ping_count: 0,
            last_incoming: Instant::now(),
            last_outgoing: Instant::now(),
            last_pkid: 0,
            inflight: 0,
            max_inflight,
            // index 0 is wasted as 0 is not a valid packet id
            outgoing_pub: vec![None; max_inflight as usize + 1],
            outgoing_rel: vec![None; max_inflight as usize + 1],
            incoming_pub: vec![None; std::u16::MAX as usize + 1],
            collision: None,
            // TODO: Optimize these sizes later
            events: VecDeque::with_capacity(100),
            write: BytesMut::with_capacity(10 * 1024),
        }
    }

    /// Returns inflight outgoing packets and clears internal queues
    pub fn clean(&mut self) -> Vec<Request> {
        let mut pending = Vec::with_capacity(100);
        // remove and collect pending publishes
        for publish in self.outgoing_pub.iter_mut() {
            if let Some(publish) = publish.take() {
                let request = Request::Publish(publish);
                pending.push(request);
            }
        }

        // remove and collect pending releases
        for rel in self.outgoing_rel.iter_mut() {
            if let Some(pkid) = rel.take() {
                let request = Request::PubRel(PubRel::new(pkid));
                pending.push(request);
            }
        }

        // remove packed ids of incoming qos2 publishes
        for id in self.incoming_pub.iter_mut() {
            id.take();
        }

        self.await_pingresp = false;
        self.collision_ping_count = 0;
        self.inflight = 0;
        pending
    }

    pub fn inflight(&self) -> u16 {
        self.inflight
    }

    /// Consolidates handling of all outgoing mqtt packet logic. Returns a packet which should
    /// be put on to the network by the eventloop
    pub fn handle_outgoing_packet(&mut self, request: Request) -> Result<(), StateError> {
        match request {
            Request::Publish(publish) => self.outgoing_publish(publish)?,
            Request::PubRel(pubrel) => self.outgoing_pubrel(pubrel)?,
            Request::Subscribe(subscribe) => self.outgoing_subscribe(subscribe)?,
            Request::Unsubscribe(unsubscribe) => self.outgoing_unsubscribe(unsubscribe)?,
            Request::PingReq => self.outgoing_ping()?,
            Request::Disconnect => self.outgoing_disconnect()?,
            _ => unimplemented!(),
        };

        self.last_outgoing = Instant::now();
        Ok(())
    }

    /// Consolidates handling of all incoming mqtt packets. Returns a `Notification` which for the
    /// user to consume and `Packet` which for the eventloop to put on the network
    /// E.g For incoming QoS1 publish packet, this method returns (Publish, Puback). Publish packet will
    /// be forwarded to user and Pubck packet will be written to network
    pub fn handle_incoming_packet(&mut self, packet: Incoming) -> Result<(), StateError> {
        let out = match &packet {
            Incoming::PingResp => self.handle_incoming_pingresp(),
            Incoming::Publish(publish) => self.handle_incoming_publish(publish),
            Incoming::SubAck(_suback) => self.handle_incoming_suback(),
            Incoming::UnsubAck(_unsuback) => self.handle_incoming_unsuback(),
            Incoming::PubAck(puback) => self.handle_incoming_puback(puback),
            Incoming::PubRec(pubrec) => self.handle_incoming_pubrec(pubrec),
            Incoming::PubRel(pubrel) => self.handle_incoming_pubrel(pubrel),
            Incoming::PubComp(pubcomp) => self.handle_incoming_pubcomp(pubcomp),
            _ => {
                error!("Invalid incoming packet = {:?}", packet);
                return Err(StateError::WrongPacket);
            }
        };

        out?;
        self.events.push_back(Event::Incoming(packet));
        self.last_incoming = Instant::now();
        Ok(())
    }

    fn handle_incoming_suback(&mut self) -> Result<(), StateError> {
        Ok(())
    }

    fn handle_incoming_unsuback(&mut self) -> Result<(), StateError> {
        Ok(())
    }

    /// Results in a publish notification in all the QoS cases. Replys with an ack
    /// in case of QoS1 and Replys rec in case of QoS while also storing the message
    fn handle_incoming_publish(&mut self, publish: &Publish) -> Result<(), StateError> {
        let qos = publish.qos;

        match qos {
            QoS::AtMostOnce => Ok(()),
            QoS::AtLeastOnce => {
                let pkid = publish.pkid;
                PubAck::new(pkid).write(&mut self.write)?;
                let event = Event::Outgoing(Outgoing::PubAck(pkid));
                self.events.push_back(event);

                Ok(())
            }
            QoS::ExactlyOnce => {
                let pkid = publish.pkid;
                PubRec::new(pkid).write(&mut self.write)?;
                self.incoming_pub[pkid as usize] = Some(pkid);
                let event = Event::Outgoing(Outgoing::PubRec(pkid));
                self.events.push_back(event);
                Ok(())
            }
        }
    }

    fn handle_incoming_puback(&mut self, puback: &PubAck) -> Result<(), StateError> {
        let v = match mem::replace(&mut self.outgoing_pub[puback.pkid as usize], None) {
            Some(_) => {
                self.inflight -= 1;
                Ok(())
            }
            None => {
                error!("Unsolicited puback packet: {:?}", puback.pkid);
                Err(StateError::Unsolicited(puback.pkid))
            }
        };

        if let Some(publish) = self.check_collision(puback.pkid) {
            self.outgoing_pub[publish.pkid as usize] = Some(publish.clone());
            self.inflight += 1;

            publish.write(&mut self.write)?;
            let event = Event::Outgoing(Outgoing::Publish(publish.pkid));
            self.events.push_back(event);
            self.collision_ping_count = 0;
        }

        v
    }

    fn handle_incoming_pubrec(&mut self, pubrec: &PubRec) -> Result<(), StateError> {
        match mem::replace(&mut self.outgoing_pub[pubrec.pkid as usize], None) {
            Some(_) => {
                // NOTE: Inflight - 1 for qos2 in comp
                self.outgoing_rel[pubrec.pkid as usize] = Some(pubrec.pkid);
                PubRel::new(pubrec.pkid).write(&mut self.write)?;

                let event = Event::Outgoing(Outgoing::PubRel(pubrec.pkid));
                self.events.push_back(event);
                Ok(())
            }
            None => {
                error!("Unsolicited pubrec packet: {:?}", pubrec.pkid);
                Err(StateError::Unsolicited(pubrec.pkid))
            }
        }
    }

    fn handle_incoming_pubrel(&mut self, pubrel: &PubRel) -> Result<(), StateError> {
        match mem::replace(&mut self.incoming_pub[pubrel.pkid as usize], None) {
            Some(_) => {
                PubComp::new(pubrel.pkid).write(&mut self.write)?;
                let event = Event::Outgoing(Outgoing::PubComp(pubrel.pkid));
                self.events.push_back(event);
                Ok(())
            }
            None => {
                error!("Unsolicited pubrel packet: {:?}", pubrel.pkid);
                Err(StateError::Unsolicited(pubrel.pkid))
            }
        }
    }

    fn handle_incoming_pubcomp(&mut self, pubcomp: &PubComp) -> Result<(), StateError> {
        if let Some(publish) = self.check_collision(pubcomp.pkid) {
            publish.write(&mut self.write)?;
            let event = Event::Outgoing(Outgoing::Publish(publish.pkid));
            self.events.push_back(event);
            self.collision_ping_count = 0;
        }

        match mem::replace(&mut self.outgoing_rel[pubcomp.pkid as usize], None) {
            Some(_) => {
                self.inflight -= 1;
                Ok(())
            }
            None => {
                error!("Unsolicited pubcomp packet: {:?}", pubcomp.pkid);
                Err(StateError::Unsolicited(pubcomp.pkid))
            }
        }
    }

    fn handle_incoming_pingresp(&mut self) -> Result<(), StateError> {
        self.await_pingresp = false;
        Ok(())
    }

    /// Adds next packet identifier to QoS 1 and 2 publish packets and returns
    /// it buy wrapping publish in packet
    fn outgoing_publish(&mut self, mut publish: Publish) -> Result<(), StateError> {
        if publish.qos != QoS::AtMostOnce {
            if publish.pkid == 0 {
                publish.pkid = self.next_pkid();
            }

            let pkid = publish.pkid;
            if self
                .outgoing_pub
                .get(publish.pkid as usize)
                .unwrap()
                .is_some()
            {
                info!("Collision on packet id = {:?}", publish.pkid);
                self.collision = Some(publish);
                let event = Event::Outgoing(Outgoing::AwaitAck(pkid));
                self.events.push_back(event);
                return Ok(());
            }

            // if there is an existing publish at this pkid, this implies that broker hasn't acked this
            // packet yet. This error is possible only when broker isn't acking sequentially
            self.outgoing_pub[pkid as usize] = Some(publish.clone());
            self.inflight += 1;
        };

        debug!(
            "Publish. Topic = {}, Pkid = {:?}, Payload Size = {:?}",
            publish.topic,
            publish.pkid,
            publish.payload.len()
        );

        publish.write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::Publish(publish.pkid));
        self.events.push_back(event);
        Ok(())
    }

    fn outgoing_pubrel(&mut self, pubrel: PubRel) -> Result<(), StateError> {
        let pubrel = self.save_pubrel(pubrel)?;

        debug!("Pubrel. Pkid = {}", pubrel.pkid);
        PubRel::new(pubrel.pkid).write(&mut self.write)?;

        let event = Event::Outgoing(Outgoing::PubRel(pubrel.pkid));
        self.events.push_back(event);
        Ok(())
    }

    /// check when the last control packet/pingreq packet is received and return
    /// the status which tells if keep alive time has exceeded
    /// NOTE: status will be checked for zero keepalive times also
    fn outgoing_ping(&mut self) -> Result<(), StateError> {
        let elapsed_in = self.last_incoming.elapsed();
        let elapsed_out = self.last_outgoing.elapsed();

        if self.collision.is_some() {
            self.collision_ping_count += 1;
            if self.collision_ping_count >= 2 {
                return Err(StateError::CollisionTimeout);
            }
        }

        // raise error if last ping didn't receive ack
        if self.await_pingresp {
            return Err(StateError::AwaitPingResp);
        }

        self.await_pingresp = true;

        debug!(
            "Pingreq,
            last incoming packet before {} millisecs,
            last outgoing request before {} millisecs",
            elapsed_in.as_millis(),
            elapsed_out.as_millis()
        );

        PingReq.write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::PingReq);
        self.events.push_back(event);
        Ok(())
    }

    fn outgoing_subscribe(&mut self, mut subscription: Subscribe) -> Result<(), StateError> {
        let pkid = self.next_pkid();
        subscription.pkid = pkid;

        debug!(
            "Subscribe. Topics = {:?}, Pkid = {:?}",
            subscription.filters, subscription.pkid
        );

        subscription.write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::Subscribe(subscription.pkid));
        self.events.push_back(event);
        Ok(())
    }

    fn outgoing_unsubscribe(&mut self, mut unsub: Unsubscribe) -> Result<(), StateError> {
        let pkid = self.next_pkid();
        unsub.pkid = pkid;

        debug!(
            "Unsubscribe. Topics = {:?}, Pkid = {:?}",
            unsub.topics, unsub.pkid
        );

        unsub.write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::Unsubscribe(unsub.pkid));
        self.events.push_back(event);
        Ok(())
    }

    fn outgoing_disconnect(&mut self) -> Result<(), StateError> {
        debug!("Disconnect");

        Disconnect.write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::Disconnect);
        self.events.push_back(event);
        Ok(())
    }

    fn check_collision(&mut self, pkid: u16) -> Option<Publish> {
        if let Some(publish) = &self.collision {
            if publish.pkid == pkid {
                return self.collision.take();
            }
        }

        None
    }

    fn save_pubrel(&mut self, mut pubrel: PubRel) -> Result<PubRel, StateError> {
        let pubrel = match pubrel.pkid {
            // consider PacketIdentifier(0) as uninitialized packets
            0 => {
                pubrel.pkid = self.next_pkid();
                pubrel
            }
            _ => pubrel,
        };

        self.outgoing_rel[pubrel.pkid as usize] = Some(pubrel.pkid);
        Ok(pubrel)
    }

    /// http://stackoverflow.com/questions/11115364/mqtt-messageid-practical-implementation
    /// Packet ids are incremented till maximum set inflight messages and reset to 1 after that.
    ///
    fn next_pkid(&mut self) -> u16 {
        let next_pkid = self.last_pkid + 1;

        // When next packet id is at the edge of inflight queue,
        // set await flag. This instructs eventloop to stop
        // processing requests until all the inflight publishes
        // are acked
        if next_pkid == self.max_inflight {
            self.last_pkid = 0;
            return next_pkid;
        }

        self.last_pkid = next_pkid;
        next_pkid
    }
}
