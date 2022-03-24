# MQuicTT

MQuicTT is a QUIC fork of [rumqtt](https://github.com/bytebeamio/rumqtt).
MQuicTT was created for my undergraduate dissertation and is a research project. 
It has not been tested nor verified for actual deployments.
So, use at your own risk, or better, just use the original `rumqtt`.

Changes made from time of fork:

* Removed rumqtt layer of TLS
* Removed TCP transport logic
* Added QUIC transport logic using QuicSocket

Importantly, some of the supporting code for `rumqtt` was removed as it was not ported/needed.
Namely, there are no more `benchmarks` and `docker`.

## Usage

To use MQuicTT you need to add it to your `Cargo.toml` using the specific git commit hash (as it is not published as a crate).
After this, you can use `rumqttc` and `rumqttd` that use QUIC as such:

```rust
// broker
use librumqttd::{Broker, Config};
use std::thread;

fn main() {
    pretty_env_logger::init();
    let config: Config = confy::load_path("rumqttd.conf").unwrap();
    let mut broker = Broker::new(config);

    let mut tx = broker.link("localclient").unwrap();
    thread::spawn(move || {
        broker.start().unwrap();
    });

    let mut rx = tx.connect(300).unwrap();
    tx.subscribe("#").unwrap();

    thread::spawn(move || {
        for i in 2..4 {
            let topic = format!(
                "some-topic",
                i
            );
            tx.publish(topic, true, "init".as_bytes()).unwrap();
        }
    });

    let mut count = 0;
    loop {
        if let Some(message) = rx.recv().unwrap() {
            count += message.payload.len();
            println!("{}", count);
        }
    }
}
```

```rust
// client
use quic_socket::{QuicClient, QuicSocket};
use rumqttc::{self, AsyncClient, MqttOptions, QoS};
use std::env;
use tokio::{task, time};
use std::time::Duration;

#[tokio::main(worker_threads = 1)]
async fn main() {
    let addr = "127.0.0.1:5000".parse().unwrap();
    pretty_env_logger::init();
    let args: Vec<String> = env::args().collect();
    let ip = &args[1];
    let client = QuicClient::new(None).await;
    let mut mqttoptions = MqttOptions::new("test-1", ip, 1883, addr, client);
    mqttoptions.set_connection_timeout(10);
    mqttoptions.set_keep_alive(5);

    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 100);
    task::spawn(async move {
        requests(client).await;
    });

    loop {
        let event = eventloop.poll().await;
        println!("{:?}", event.unwrap());
    }
}

async fn requests(client: AsyncClient) {
    client
        .subscribe("some-topic", QoS::AtMostOnce)
        .await
        .unwrap();
    let payload = "test".as_bytes();
    for _ in 1..=100 {
        client
            .publish("some-topic", QoS::ExactlyOnce, false, payload)
            .await
            .unwrap();

        time::sleep(Duration::from_secs(1)).await;
    }

    time::sleep(Duration::from_secs(120)).await;
}
```

Importantly, you need to generate the TLS certificates and launch the broker and client from directories where they are present.
