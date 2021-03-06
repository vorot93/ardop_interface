//! Framing for the TNC control protocol
//!
use std::string::String;

use bytes::{Buf, BytesMut};
use futures_codec::{Decoder, Encoder};

use crate::protocol::response::Response;

/// Frames and sends TNC control messages
pub struct TncControlFraming {}

impl TncControlFraming {
    /// New TNC control message framer
    pub fn new() -> TncControlFraming {
        TncControlFraming {}
    }
}

impl Encoder for TncControlFraming {
    type Item = String;
    type Error = std::io::Error;

    fn encode(&mut self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.extend_from_slice(item.as_bytes());
        Ok(())
    }
}

impl Decoder for TncControlFraming {
    type Item = Response;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // parse the head of src
        let res = Response::parse(src.as_ref());

        // drop parsed characters from the buffer
        let _ = src.advance(res.0);

        match &res.1 {
            Some(ref resp) => trace!("Control received: {:?}", resp),
            _ => (),
        }

        Ok(res.1)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::str;

    use futures::executor;
    use futures::io::Cursor;
    use futures::prelude::*;

    use futures_codec::Framed;

    use crate::protocol::response::{Event, Response};

    #[test]
    fn test_encode_decode() {
        let words = b"PENDING\rCANCELPENDING\r".to_vec();
        let curs = Cursor::new(words);
        let mut framer = Framed::new(curs, TncControlFraming::new());

        executor::block_on(async {
            let e1 = framer.next().await;
            assert_eq!(Response::Event(Event::PENDING), e1.unwrap().unwrap());

            let e2 = framer.next().await;
            assert_eq!(Response::Event(Event::CANCELPENDING), e2.unwrap().unwrap());

            let e3 = framer.next().await;
            assert!(e3.is_none());
        });

        let curs = Cursor::new(vec![0u8; 24]);
        let mut framer = Framed::new(curs, TncControlFraming::new());

        executor::block_on(async {
            framer.send("MYCALL W1AW\r".to_owned()).await.unwrap();
            framer.send("LISTEN TRUE\r".to_owned()).await.unwrap();
        });
        let (curs, _) = framer.release();
        assert_eq!(
            "MYCALL W1AW\rLISTEN TRUE\r",
            str::from_utf8(curs.into_inner().as_ref()).unwrap()
        );
    }
}
