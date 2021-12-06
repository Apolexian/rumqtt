use bytes::{BufMut, BytesMut};
use mqttbytes::v4::*;
use mqttbytes::*;

use crate::{Incoming, MqttState, StateError};
use quic_socket::{QuicMessage, QuicSocket};
use std::io;
/// Network transforms packets <-> frames efficiently. It takes
/// advantage of pre-allocation, buffering and vectorization when
/// appropriate to achieve performance
pub struct Network {
    quic: QuicMessage,
    read: BytesMut,
    max_incoming_size: usize,
    max_readb_count: usize,
}

impl Network {
    pub fn new(quic: QuicMessage, max_incoming_size: usize) -> Network {
        Network {
            quic,
            read: BytesMut::with_capacity(10 * 1024),
            max_incoming_size,
            max_readb_count: 10,
        }
    }

    /// Reads more than 'required' bytes to frame a packet into self.read buffer
    async fn read_bytes(&mut self, required: usize) -> io::Result<usize> {
        let mut total_read = 0;
        loop {
            let read = self.quic.server.recv().await.unwrap();
            let mut bytes_read = BytesMut::new();
            bytes_read.put(read.as_slice());
            self.read = bytes_read;
            let read_len = read.len();
            if 0 == read_len {
                return if self.read.is_empty() {
                    Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "connection closed by peer",
                    ))
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "connection reset by peer",
                    ))
                };
            }

            total_read += read_len;
            if total_read >= required {
                return Ok(total_read);
            }
        }
    }

    pub async fn read(&mut self) -> Result<Incoming, io::Error> {
        loop {
            let required = match read(&mut self.read, self.max_incoming_size) {
                Ok(packet) => return Ok(packet),
                Err(mqttbytes::Error::InsufficientBytes(required)) => required,
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            };

            // read more packets until a frame can be created. This function
            // blocks until a frame can be created. Use this in a select! branch
            self.read_bytes(required).await?;
        }
    }

    /// Read packets in bulk. This allow replies to be in bulk. This method is used
    /// after the connection is established to read a bunch of incoming packets
    pub async fn readb(&mut self, state: &mut MqttState) -> Result<(), StateError> {
        let mut count = 0;
        loop {
            match read(&mut self.read, self.max_incoming_size) {
                Ok(packet) => {
                    state.handle_incoming_packet(packet)?;

                    count += 1;
                    if count >= self.max_readb_count {
                        return Ok(());
                    }
                }
                // If some packets are already framed, return those
                Err(Error::InsufficientBytes(_)) if count > 0 => return Ok(()),
                // Wait for more bytes until a frame can be created
                Err(Error::InsufficientBytes(required)) => {
                    self.read_bytes(required).await?;
                }
                Err(e) => return Err(StateError::Deserialization(e)),
            };
        }
    }

    pub async fn connect(&mut self, connect: Connect) -> Result<usize, io::Error> {
        let mut write = BytesMut::with_capacity(65535);
        let len = match connect.write(&mut write) {
            Ok(size) => size,
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        };
        self.quic.client.send(write[..].to_vec()).await.unwrap();
        Ok(len)
    }

    pub async fn flush(&mut self, write: &mut BytesMut) -> Result<(), io::Error> {
        if write.is_empty() {
            return Ok(());
        }
        self.quic.client.send(write[..].to_vec()).await.unwrap();
        write.clear();
        Ok(())
    }
}
