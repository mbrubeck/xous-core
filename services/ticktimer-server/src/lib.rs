#![cfg_attr(target_os = "none", no_std)]

pub mod api;

use xous::{try_send_message, CID, Error};

pub fn elapsed_ms(cid: CID) -> Result<u64, Error> {
    let response = try_send_message(cid, api::Opcode::ElapsedMs.into())?;
    if let xous::Result::Scalar2(upper, lower) = response {
       Ok(upper as u64 |  ((lower as u64) << 32))
    } else {
       panic!("unexpected return value: {:#?}", response);
    }
    /* // scalar1 experimental reply to see if this makes it any better. It doesn't.
    if let xous::Result::Scalar1(lower) = response {
        Ok(lower as u64)
     } else {
        panic!("unexpected return value: {:#?}", response);
     }
     */
 }

pub fn reset(cid: CID) -> Result<(), xous::Error> {
    try_send_message(cid, api::Opcode::Reset.into()).map(|_| ())
}
