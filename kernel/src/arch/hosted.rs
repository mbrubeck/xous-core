pub mod irq;
pub mod mem;
pub mod process;
pub mod syscall;

use std::cell::RefCell;
use std::convert::TryInto;
use std::env;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread_local;

use crate::arch::process::Process;
use crate::services::SystemServices;

use xous::{MemoryAddress, ProcessInit, ProcessKey, Result, SysCall, PID, TID};

enum ThreadMessage {
    SysCall(PID, TID, SysCall),
    NewConnection(TcpStream, ProcessKey),
}

#[derive(Debug)]
enum NewPidMessage {
    NewPid(PID),
}

#[derive(Debug)]
enum ExitMessage {
    Exit,
}

thread_local!(static NETWORK_LISTEN_ADDRESS: RefCell<SocketAddr> = RefCell::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0)));
thread_local!(static SEND_ADDR: RefCell<Option<Sender<SocketAddr>>> = RefCell::new(None));
thread_local!(static PID1_KEY: RefCell<[u8; 16]> = RefCell::new([0u8; 16]));

#[cfg(test)]
pub fn set_pid1_key(new_key: [u8; 16]) {
    PID1_KEY.with(|p1k| *p1k.borrow_mut() = new_key);
}

/// Set the network address for this particular thread.
#[cfg(test)]
pub fn set_listen_address(new_address: &SocketAddr) {
    NETWORK_LISTEN_ADDRESS.with(|nla| {
        let mut address = nla.borrow_mut();
        *address = *new_address;
    });
}

/// Set the network address for this particular thread.
#[allow(dead_code)]
pub fn set_send_addr(send_addr: Sender<SocketAddr>) {
    SEND_ADDR.with(|sa| {
        *sa.borrow_mut() = Some(send_addr);
    });
}

#[cfg(not(feature = "testing"))]
fn generate_pid_key() -> [u8; 16] {
    use rand::{thread_rng, Rng};
    let mut process_key = [0u8; 16];
    let mut rng = thread_rng();
    for b in process_key.iter_mut() {
        *b = rng.gen();
    }
    process_key
}

/// Each client gets its own connection and its own thread, which is handled here.
fn handle_connection(
    conn: TcpStream,
    pid: PID,
    chn: Sender<ThreadMessage>,
    should_exit: std::sync::Arc<core::sync::atomic::AtomicBool>,
) {
    enum ServerMessage {
        Exit,
        ServerPacket([usize; 9]),
        ServerPacketWithData([usize; 9], Vec<u8>),
    }

    fn conn_thread(mut conn: TcpStream, sender: Sender<ServerMessage>) {
        loop {
            let mut raw_data = [0u8; 9 * std::mem::size_of::<usize>()];
            if let Err(_e) = conn.read_exact(&mut raw_data) {
                // println!(
                //     "KERNEL(?): Client disconnected: {} ({:?}). Shutting down virtual process.",
                //     _e, _e
                // );
                sender.send(ServerMessage::Exit).ok();
                return;
            }

            let mut packet_data = [0usize; 9];
            for (bytes, word) in raw_data
                .chunks_exact(std::mem::size_of::<usize>())
                .zip(packet_data.iter_mut())
            {
                *word = usize::from_le_bytes(bytes.try_into().unwrap());
            }

            if packet_data[1] == 16
                && (packet_data[3] == 1 || packet_data[3] == 2 || packet_data[3] == 3)
            {
                let mut v = vec![0; packet_data[6]];
                if conn.read_exact(&mut v).is_err() {
                    sender.send(ServerMessage::Exit).ok();
                    return;
                }
                sender
                    .send(ServerMessage::ServerPacketWithData(packet_data, v))
                    .unwrap();
            } else {
                sender
                    .send(ServerMessage::ServerPacket(packet_data))
                    .unwrap();
            }
        }
    }

    let (sender, receiver) = channel();
    let conn_sender = sender.clone();
    std::thread::Builder::new()
        .name(format!("PID {}: client connection thread", pid))
        .spawn(move || {
            conn_thread(conn, conn_sender);
        })
        .unwrap();

    std::thread::Builder::new()
        .name(format!("PID {}: client should_exit thread", pid))
        .spawn(move || loop {
            if should_exit.load(core::sync::atomic::Ordering::Relaxed) {
                // eprintln!("KERNEL: should_exit == 1");
                sender.send(ServerMessage::Exit).ok();
                return;
            }
            std::thread::park_timeout(std::time::Duration::from_secs(1));
        })
        .unwrap();

    for msg in receiver {
        match msg {
            ServerMessage::Exit => break,
            ServerMessage::ServerPacket(pkt) => {
                let thread_id = pkt[0];
                let call = xous::SysCall::from_args(
                    pkt[1], pkt[2], pkt[3], pkt[4], pkt[5], pkt[6], pkt[7], pkt[8],
                );
                match call {
                    Err(e) => {
                        eprintln!("KERNEL({}): Received invalid syscall: {:?}", pid, e);
                        eprintln!(
                            "Raw packet: {:08x} {} {} {} {} {} {} {}",
                            pkt[0], pkt[1], pkt[2], pkt[3], pkt[4], pkt[5], pkt[6], pkt[7]
                        );
                    }
                    Ok(call) => chn
                        .send(ThreadMessage::SysCall(pid, thread_id, call))
                        .expect("couldn't make syscall"),
                }
            }
            ServerMessage::ServerPacketWithData(pkt, data) => {
                let thread_id = pkt[0];
                let call = xous::SysCall::from_args(
                    pkt[1], pkt[2], pkt[3], pkt[4], pkt[5], pkt[6], pkt[7], pkt[8],
                );
                match call {
                    Err(e) => {
                        eprintln!("KERNEL({}): Received invalid syscall: {:?}", pid, e);
                        eprintln!(
                            "Raw packet: {:08x} {} {} {} {} {} {} {}",
                            pkt[0], pkt[1], pkt[2], pkt[3], pkt[4], pkt[5], pkt[6], pkt[7]
                        );
                    }
                    Ok(mut call) => {
                        // eprintln!(
                        //     "Received packet: {:08x} {} {} {} {} {} {} {}: {:?}",
                        //     pkt[0], pkt[1], pkt[2], pkt[3], pkt[4], pkt[5], pkt[6], pkt[7], call
                        // );
                        if let SysCall::SendMessage(ref _cid, ref mut envelope) = call {
                            match envelope {
                                xous::Message::MutableBorrow(msg)
                                | xous::Message::Borrow(msg)
                                | xous::Message::Move(msg) => {
                                    // Update the address pointer. This will get turned back into a
                                    // usable pointer by casting it back into a &[T] on the other
                                    // side. This is just a pointer to the start of data
                                    // as well as the index into the data it points at. The lengths
                                    // should still be equal once we reconstitute the data in the
                                    // other process.
                                    // ::debug_here::debug_here!();
                                    let sliced_data = data.into_boxed_slice();
                                    assert_eq!(
                                        sliced_data.len(),
                                        msg.buf.len(),
                                        "deconstructed data {} != message buf length {}",
                                        sliced_data.len(),
                                        msg.buf.len()
                                    );
                                    msg.buf.addr =
                                        match MemoryAddress::new(Box::into_raw(sliced_data)
                                            as *mut u8
                                            as usize)
                                        {
                                            Some(a) => a,
                                            _ => unreachable!(),
                                        };
                                }
                                xous::Message::Scalar(_) => (),
                            }
                        } else {
                            panic!("unsupported message type");
                        }
                        chn.send(ThreadMessage::SysCall(pid, thread_id, call))
                            .expect("couldn't make syscall");
                    }
                }
            }
        }
    }
    // eprintln!("KERNEL({}): Finished the thread so sending TerminateProcess", pid);
    chn.send(ThreadMessage::SysCall(
        pid,
        1,
        xous::SysCall::TerminateProcess,
    ))
    .unwrap();
}

fn listen_thread(
    listen_addr: SocketAddr,
    chn: Sender<ThreadMessage>,
    mut local_addr_sender: Option<Sender<SocketAddr>>,
    new_pid_channel: Receiver<NewPidMessage>,
    exit_channel: Receiver<ExitMessage>,
) {
    let should_exit = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));

    // println!("KERNEL(1): Starting Xous server on {}...", listen_addr);
    let listener = TcpListener::bind(listen_addr).unwrap_or_else(|e| {
        panic!("Unable to create server: {}", e);
    });
    // Notify the host what our kernel address is, if a listener exists.
    if let Some(las) = local_addr_sender.take() {
        las.send(listener.local_addr().unwrap()).unwrap();
    }

    let mut clients = vec![];

    fn accept_new_connection(
        mut conn: TcpStream,
        chn: &Sender<ThreadMessage>,
        new_pid_channel: &Receiver<NewPidMessage>,
        clients: &mut Vec<(std::thread::JoinHandle<()>, TcpStream)>,
        should_exit: &std::sync::Arc<core::sync::atomic::AtomicBool>,
    ) -> bool {
        let thr_chn = chn.clone();

        // Read the challenge access key from the client
        let mut access_key = [0u8; 16];
        conn.read_exact(&mut access_key).unwrap();

        // Spawn a new process. This process will start out in the "Setup()" state.
        chn.send(ThreadMessage::NewConnection(
            conn.try_clone()
                .expect("couldn't make a copy of the network connection for the kernel"),
            ProcessKey::new(access_key),
        ))
        .expect("couldn't request a new PID");
        let NewPidMessage::NewPid(new_pid) = new_pid_channel
            .recv()
            .expect("couldn't receive message from main thread");
        // println!("KERNEL({}): New client connected from {}", new_pid, _addr);
        let conn_copy = conn.try_clone().expect("couldn't duplicate connection");
        let should_exit = should_exit.clone();
        let jh = std::thread::Builder::new()
            .name(format!("kernel PID {} listener", new_pid))
            .spawn(move || handle_connection(conn, new_pid, thr_chn, should_exit))
            .expect("couldn't spawn listen thread");
        clients.push((jh, conn_copy));
        false
    }

    fn exit_server(
        should_exit: std::sync::Arc<core::sync::atomic::AtomicBool>,
        clients: Vec<(std::thread::JoinHandle<()>, TcpStream)>,
    ) {
        should_exit.store(true, core::sync::atomic::Ordering::Relaxed);
        for (jh, conn) in clients {
            use std::net::Shutdown;
            conn.shutdown(Shutdown::Both)
                .expect("couldn't shutdown client");
            jh.join().expect("couldn't join client thread");
        }
    }

    // Use `listener` in a nonblocking setup so that we can exit when doing tests
    enum ClientMessage {
        NewConnection(TcpStream),
        Exit,
    };
    let (sender, receiver) = channel();
    let tcp_sender = sender.clone();
    let exit_sender = sender;

    let (shutdown_listener, shutdown_listener_receiver) = channel();

    // `listener.accept()` has no way to break, so we must put it in nonblocking mode
    listener.set_nonblocking(true).unwrap();

    std::thread::Builder::new()
        .name("kernel accept thread".to_owned())
        .spawn(move || loop {
            match listener.accept() {
                Ok((conn, _addr)) => {
                    conn.set_nonblocking(false).unwrap();
                    tcp_sender.send(ClientMessage::NewConnection(conn)).unwrap();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    match shutdown_listener_receiver
                        .recv_timeout(std::time::Duration::from_millis(500))
                    {
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            return;
                        }
                    }
                }
                Err(e) => {
                    // Windows generates this error -- WSACancelBlockingCall -- when a
                    // connection is shut down while `accept()` is running. This should
                    // only happen when the system is shutting down, so ignore it.
                    if cfg!(windows) {
                        if let Some(10004) = e.raw_os_error() {
                            return;
                        }
                    }
                    eprintln!(
                        "error accepting connections: {} ({:?}) ({:?})",
                        e,
                        e,
                        e.kind()
                    );
                    return;
                }
            }
        })
        .unwrap();

    // Spawn a thread to listen for the `exit` command, and relay that
    // to the main thread. This prevents us from needing to poll, since
    // all messages are coalesced into a single channel.
    std::thread::Builder::new()
        .name("kernel exit listener".to_owned())
        .spawn(move || match exit_channel.recv() {
            Ok(ExitMessage::Exit) => exit_sender.send(ClientMessage::Exit).unwrap(),
            Err(std::sync::mpsc::RecvError) => eprintln!("error receiving exit command"),
        })
        .unwrap();

    for msg in receiver {
        match msg {
            ClientMessage::NewConnection(conn) => {
                if accept_new_connection(conn, &chn, &new_pid_channel, &mut clients, &should_exit) {
                    break;
                }
            }
            ClientMessage::Exit => break,
        }
    }
    shutdown_listener.send(()).unwrap();
    exit_server(should_exit, clients);
}

/// The idle function is run when there are no directly-runnable processes
/// that kmain can activate. In a hosted environment,this is the primary
/// thread that handles network communications, and this function never returns.
pub fn idle() -> bool {
    // Start listening.
    let (sender, message_receiver) = channel();
    let (new_pid_sender, new_pid_receiver) = channel();
    let (exit_sender, exit_receiver) = channel();

    // Allocate PID1 with the key we were passed.
    let pid1_key = PID1_KEY.with(|p1k| *p1k.borrow());
    let pid1_init = ProcessInit {
        key: ProcessKey::new(pid1_key),
    };
    let pid1 = SystemServices::with_mut(|ss| ss.create_process(pid1_init)).unwrap();
    assert_eq!(pid1.get(), 1);

    let listen_addr = env::var("XOUS_LISTEN_ADDR")
        .map(|s| {
            s.to_socket_addrs()
                .expect("invalid server address")
                .next()
                .expect("unable to resolve server address")
        })
        .unwrap_or_else(|_| NETWORK_LISTEN_ADDRESS.with(|nla| *nla.borrow()));

    #[cfg(not(feature = "testing"))]
    let address_receiver = {
        let (sender, receiver) = channel();
        set_send_addr(sender);
        receiver
    };

    let listen_thread_handle = SEND_ADDR.with(|sa| {
        let sa = sa.borrow_mut().take();
        std::thread::Builder::new()
            .name("kernel network listener".to_owned())
            .spawn(move || listen_thread(listen_addr, sender, sa, new_pid_receiver, exit_receiver))
            .expect("couldn't spawn listen thread")
    });

    #[cfg(not(feature = "testing"))]
    {
        let address = address_receiver.recv().unwrap();
        xous::arch::set_xous_address(address);
        println!("KERNEL: Xous server listening on {}", address);
        println!("KERNEL: Starting initial processes:");
        let mut args = std::env::args();
        args.next();

        // Set the current PID to 1, which was created above. This ensures all init processes
        // are owned by PID1.
        crate::arch::process::set_current_pid(pid1);

        // Go through each arg and spawn it as a new process. Failures here will
        // halt the entire system.
        println!("  PID  |  Command");
        println!("-------+------------------");
        for arg in args {
            let process_key = generate_pid_key();
            let init = xous::ProcessInit {
                key: ProcessKey::new(process_key),
            };
            let new_pid = SystemServices::with_mut(|ss| ss.create_process(init)).unwrap();
            println!(" {:^5} |  {}", new_pid, arg);
            let process_args = xous::ProcessArgs::new("program", arg);
            xous::arch::create_process_post(process_args, init, new_pid).expect("couldn't spawn");
        }
    }

    while let Ok(msg) = message_receiver.recv() {
        match msg {
            ThreadMessage::NewConnection(conn, access_key) => {
                // The new process should already have a PID registered. Convert its access key
                // into a PID, and register the connection with the server.
                let new_pid =
                    crate::arch::process::register_connection_for_key(conn, access_key).unwrap();
                // println!(
                //     "KERNEL: Access key {:?} mapped to PID {}",
                //     access_key, new_pid
                // );

                // Inform the backchannel of the new process ID.
                new_pid_sender
                    .send(NewPidMessage::NewPid(new_pid))
                    .expect("couldn't send new pid to new connection");

                // conn.write_all(&new_pid.get().to_le_bytes())
                //     .expect("couldn't send pid to new process");

                // Switch to this process immediately, which moves it from `Setup(_)` to `Running(0)`.
                // Note that in this system, multiple processes can be active at once. This is
                // similar to having one core for each process
                // SystemServices::with_mut(|ss| ss.switch_to_thread(new_pid, Some(1))).unwrap();
            }
            ThreadMessage::SysCall(pid, thread_id, call) => {
                // println!("KERNEL({}): Received syscall {:?}", pid, call);
                crate::arch::process::set_current_pid(pid);
                // println!("KERNEL({}): Now running as the new process", pid);

                // If the call being made is to terminate the current process, we need to know
                // because we won't be able to send a response.
                let is_terminate = call == SysCall::TerminateProcess;
                let is_shutdown = call == SysCall::Shutdown;

                // For a "Shutdown" command, send the response before we issue the shutdown.
                // This is because the "process" will be "terminated" (the network socket will be closed),
                // and we won't be able to send the response after we're done.
                if is_shutdown {
                    // println!("KERNEL: Detected shutdown -- sending final \"Ok\" to the client");
                    let mut process = Process::current();
                    let mut response_vec = Vec::new();
                    response_vec.extend_from_slice(&thread_id.to_le_bytes());
                    for word in Result::Ok.to_args().iter_mut() {
                        response_vec.extend_from_slice(&word.to_le_bytes());
                    }
                    process.send(&response_vec).unwrap_or_else(|_e| {
                        // If we're unable to send data to the process, assume it's dead and terminate it.
                        println!(
                            "Unable to send response to process: {:?} -- terminating",
                            _e
                        );
                        crate::syscall::handle(pid, thread_id, SysCall::TerminateProcess).ok();
                    });
                    // println!("KERNEL: Done sending");
                }

                // Handle the syscall within the Xous kernel
                let response =
                    crate::syscall::handle(pid, thread_id, call).unwrap_or_else(Result::Error);

                // println!("KERNEL({}): Syscall response {:?}", pid, response);
                // There's a response if it wasn't a blocked process and we're not terminating.
                // Send the response back to the target.
                if response != Result::BlockedProcess && !is_terminate && !is_shutdown {
                    // The syscall may change what the current process is, but we always
                    // want to send a response to the process where the request came from.
                    // For this block, switch to the original PID, send the message, then
                    // switch back.
                    let existing_pid = crate::arch::process::current_pid();
                    crate::arch::process::set_current_pid(pid);

                    let mut process = Process::current();
                    let mut response_vec = Vec::new();
                    response_vec.extend_from_slice(&thread_id.to_le_bytes());
                    for word in response.to_args().iter_mut() {
                        response_vec.extend_from_slice(&word.to_le_bytes());
                    }
                    process.send(&response_vec).unwrap_or_else(|_e| {
                        // If we're unable to send data to the process, assume it's dead and terminate it.
                        eprintln!(
                            "KERNEL({}): Unable to send response to process: {:?} -- terminating",
                            pid, _e
                        );
                        crate::syscall::handle(pid, thread_id, SysCall::TerminateProcess).ok();
                    });
                    crate::arch::process::set_current_pid(existing_pid);
                    // SystemServices::with_mut(|ss| {
                    // ss.switch_from(pid, 1, true)}).unwrap();
                }

                if is_shutdown {
                    exit_sender
                        .send(ExitMessage::Exit)
                        .expect("couldn't send shutdown signal");
                    break;
                }
            }
        }
    }

    // println!("Exiting Xous because the listen thread channel has closed. Waiting for thread to finish...");
    listen_thread_handle
        .join()
        .expect("error waiting for listen thread to return");

    // println!("Thank you for using Xous!");
    false
}