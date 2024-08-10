<h2>
  RoboPLC I/O connector for IEC 60870-5
  <a href="https://crates.io/crates/roboplc-io-iec60870-5"><img alt="crates.io page" src="https://img.shields.io/crates/v/roboplc-io-iec60870-5.svg"></img></a>
  <a href="https://docs.rs/roboplc-io-iec60870-5"><img alt="docs.rs page" src="https://docs.rs/roboplc-io-iec60870-5/badge.svg"></img></a>
</h2>

# Introduction

[IEC 60870-5](https://en.wikipedia.org/wiki/IEC_60870-5) is a set of standards
for telecontrol, teleprotection, and associated telecommunications for electric
power systems, widely used in the European Union, the United Kingdom and other
locations.

This crate provides I/O connector for [RoboPLC](https://www.roboplc.com/).

The crate IS NOT FREE for any commercial or production use. Please refer to
<https://github.com/roboplc/roboplc-io-iec60870-5/blob/main/LICENSE.md> for
more information.

The client additionally supports:

- Auto-reconnects
- Multi-threading
- Real-time safety
- Enterprise support from the vendor

Note: as the client has got an asynchronous-manner reader loop, it is HIGHLY RECOMMENDED to use
timeouts. In case if a remote does not respond, a request with no timeout gets stuck forever.

# Example

## IEC 60870-5 101 (Serial)

The crate does not provide any client for IEC 60870-5 101 (Serial) as such does
not require re-connection. Any communication library plus [IEC
60870-5](https://crates.io/crates/iec60870-5) crate can be used to create/parse
IEC 60870-5 101 telegrams.

```rust,no_run

## IEC 60870-5 104 (TCP)

## Connecting a client

```rust,no_run
use roboplc::comm::Timeouts;
use roboplc_io_iec60870_5::iec104::{Client, PingKind};
use std::time::Duration;

// Create a new IEC 60870-5 104 client
let (client, reader) = Client::new("192.168.1.100:2404", Timeouts::default(), 1024).unwrap();
// Get the telegram receiver
let telegram_rx = reader.get_telegram_receiver();
// The reader must be run in a separate thread
std::thread::spawn(move || reader.run());
// Create an optional pinger worker, which can send test/ack frames or
// automatically re-connect the socket
let pinger = client.pinger(PingKind::Test, Duration::from_secs(1));
std::thread::spawn(move || pinger.run());
```

### Handling incoming telegrams

```rust,ignore
use iec60870_5::{
    telegram104::{Telegram104, Telegram104_I},
    types::{datatype::{DataType, M_EP_TA_1}, COT},
};
use roboplc::prelude::*;
use std::time::Duration;

while let Ok(telegram) = telegram_rx.recv() {
    println!("{:?}", telegram);
    if let Telegram104::I(i) = telegram {
        if i.data_type() == DataType::M_EP_TA_1 && i.cot() == COT::Cyclic {
            for iou in i.iou() {
                let v: M_EP_TA_1 = iou.value().into();
                let dt = Timestamp::try_from(v.time)
                    .unwrap()
                    .try_into_datetime_local()
                    .unwrap();
                dbg!(v.sep.es, Duration::from(v.elapsed), dt);
            }
        }
    }
}
```

### Sending telegrams

The client provides two methods to send telegrams:

- [`iec104::Client::send`] for sending a telegram with no reply expected
- [`iec104::Client::command`] for sending a command telegram and waiting for a reply

```rust,ignore
use iec60870_5::{
    telegram104::{Telegram104, Telegram104_I},
    types::{datatype::{DataType, SelectExecute, C_RC_TA_1, QU, RCO, RCS}, COT},
};

use roboplc::prelude::*;
let mut telegram: Telegram104_I = Telegram104_I::new(DataType::C_RC_TA_1, COT::Act, 47);
telegram.append_iou(
    12,
    C_RC_TA_1 {
        rco: RCO {
            rcs: RCS::Increment,
            se: SelectExecute::Execute,
            qu: QU::Persistent,
        },
        time: Timestamp::now().try_into().unwrap(),
    },
);
let request: Telegram104 = telegram.into();
let reply = client.command(request).unwrap();
println!("COMMAND REPLY: {:?}", reply);
```

## Locking policy

The crate locking policy is set using the same features as RoboPLC locking
policy. The selected policy must be the same.
