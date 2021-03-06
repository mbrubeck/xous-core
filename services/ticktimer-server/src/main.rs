#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(target_os = "none")]
#[macro_use]
mod debug;

mod api;
use api::Opcode;

use core::convert::TryFrom;


#[cfg(target_os = "none")]
mod implementation {
    const TICKS_PER_MS: u64 = 1;
    use utralib::generated::*;
    pub struct XousTickTimer {
        csr: xous::MemoryRange,
    }

    impl XousTickTimer {
        pub fn new() -> XousTickTimer {
            let ctrl = xous::syscall::map_memory(
                xous::MemoryAddress::new(utra::ticktimer::HW_TICKTIMER_BASE),
                None,
                4096,
                xous::MemoryFlags::R | xous::MemoryFlags::W,
            )
            .expect("couldn't map Tick Timer CSR range");

            XousTickTimer { csr: ctrl }
        }

        pub fn reset(&mut self) {
            let mut tt = CSR::new(self.csr.as_mut_ptr() as *mut u32);
            tt.wfo(utra::ticktimer::CONTROL_RESET, 0b1);
            tt.wo(utra::ticktimer::CONTROL, 0); // not paused, not reset -> free-run
        }

        pub fn raw_ticktime(&self) -> u64 {
            let tt = CSR::new(self.csr.as_mut_ptr() as *mut u32);
            let mut time: u64 = tt.r(utra::ticktimer::TIME0) as u64;
            time |= (tt.r(utra::ticktimer::TIME1) as u64) << 32;

            time
        }

        pub fn elapsed_ms(&self) -> u64 {
            self.raw_ticktime() / TICKS_PER_MS
        }
    }
}

#[cfg(not(target_os = "none"))]
mod implementation {
    use std::convert::TryInto;
    pub struct XousTickTimer {
        start: std::time::Instant,
    }

    impl XousTickTimer {
        pub fn new() -> XousTickTimer {
            XousTickTimer {
                start: std::time::Instant::now(),
            }
        }

        pub fn reset(&mut self) {
            self.start = std::time::Instant::now();
        }

        pub fn elapsed_ms(&self) -> u64 {
            self.start.elapsed().as_millis().try_into().unwrap()
        }
    }
}

use implementation::*;

#[xous::xous_main]
fn xmain() -> ! {
    // Create a new ticktimer object
    let mut ticktimer = XousTickTimer::new();

    let ticktimer_server =
        xous::create_server(b"ticktimer-server").expect("Couldn't create Ticktimer server");
    loop {
        println!("TickTimer: waiting for message");
        let envelope = xous::receive_message(ticktimer_server).unwrap();
        println!("TickTimer: Message: {:?}", envelope);
        if let Ok(opcode) = Opcode::try_from(&envelope.body) {
            println!("TickTimer: Opcode: {:?}", opcode);
            match opcode {
                Opcode::Reset => {
                    println!("TickTimer: reset called");
                    ticktimer.reset();
                }
                Opcode::ElapsedMs => {
                    let time = ticktimer.elapsed_ms();
                    println!("TickTimer: returning time of {:?}", time);
                    xous::return_scalar2(
                        envelope.sender,
                        (time & 0xFFFF_FFFFu64) as usize,
                        ((time >> 32) & 0xFFF_FFFFu64) as usize,
                    )
                    .expect("TickTimer: couldn't return time request");
                    println!("TickTimer: done returning value");
                }
            }
        } else {
            println!("Couldn't convert opcode");
        }
    }
}
