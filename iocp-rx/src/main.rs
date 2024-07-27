use core::slice;
use std::alloc;
use std::ops::Deref;
use std::sync::mpsc::sync_channel;
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::{self};
use windows::core::{s, PCSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Pipes::*;
use windows::Win32::System::Threading::*;
use windows::Win32::System::IO::*;

use iocp_shared::*;

const PIPE_NAME: PCSTR = s!("\\\\.\\pipe\\iocp_example_pipe");
const BUFFER_SIZE: usize = 512;
const THREAD_POOL_SIZE: usize = 4;
const PIPE_INSTANCES_PER_WORKER: usize = 10;

/// This HANDLE must be safe to use from multiple threads. See: https://github.com/microsoft/windows-rs/issues/3093
#[derive(Copy, Clone, Debug)]
pub struct SyncHandle(pub HANDLE);

unsafe impl Sync for SyncHandle {}
unsafe impl Send for SyncHandle {}

impl Deref for SyncHandle {
    type Target = HANDLE;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn main() {
    let iocp = create_io_completion_port();
    let mut pool = IocpWorkerPool::new(iocp);
    // This blocks forever.
    // You can shutdown this pool by closing iocp handle.
    // All workers will catch this and return causing listener thread to return as well.
    pool.start_workers_and_accept_connections();
}

fn create_io_completion_port() -> SyncHandle {
    let iocp = unsafe {
        CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, THREAD_POOL_SIZE as _).unwrap()
    };
    SyncHandle(iocp)
}

struct IocpWorkerPool {
    io_objects_pool: Receiver<usize>,
    io_objects_release_channel: Option<SyncSender<usize>>,

    // Alias to allocation holding all `IoData` objects. It is accessed only in drop impl and thus
    // is safe because drop is guaranteed to be run exclusively by one thread.
    allocation: usize,
    pipe_instance_count: usize,
    iocp: SyncHandle,
}

impl IocpWorkerPool {
    fn new(iocp: SyncHandle) -> Self {
        let pipe_instance_count = PIPE_INSTANCES_PER_WORKER * THREAD_POOL_SIZE;
        let (io_objects_tx, io_objects_rx) = sync_channel(pipe_instance_count);

        let (allocation, slice) = unsafe {
            let layout =
                alloc::Layout::array::<IoData>(pipe_instance_count).expect("arguments are correct");
            let mem = alloc::alloc_zeroed(layout);
            let s = slice::from_raw_parts_mut::<IoData>(mem as *mut _, pipe_instance_count);
            (mem as usize, s)
        };

        for slot in slice.iter_mut() {
            let pipe = unsafe {
                CreateNamedPipeA(
                    PIPE_NAME,
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    BUFFER_SIZE as u32,
                    BUFFER_SIZE as u32,
                    0,
                    None,
                )
            }
            .unwrap();

            slot.pipe = pipe;
            let io_data_ptr = slot as *mut _;

            unsafe {
                CreateIoCompletionPort(pipe, *iocp, io_data_ptr as usize, 0).unwrap();
            }

            io_objects_tx
                .send(io_data_ptr as _)
                .expect("queue is big enough to hold every io_data");
        }

        Self {
            io_objects_pool: io_objects_rx,
            io_objects_release_channel: Some(io_objects_tx),

            allocation,
            iocp,
            pipe_instance_count,
        }
    }

    fn start_workers_and_accept_connections(&mut self) {
        let io_objects_release_channel = self
            .io_objects_release_channel
            .take()
            .expect("channel was initialized");
        thread::scope(|s| {
            for _ in 0..THREAD_POOL_SIZE {
                let release_channel = io_objects_release_channel.clone();
                let iocp = self.iocp;
                s.spawn(move || unsafe { handle_iocp(iocp, release_channel) });
            }

            // We drop this sender so that only senders left are ones owned by woker threads, this
            // enables the following loop to exit once all workers quit.
            drop(io_objects_release_channel);

            while let Some(io_data) = self.io_objects_pool.iter().next() {
                unsafe {
                    let io_data = &mut *(io_data as *mut IoData);
                    IoData::reset(io_data);
                    let pipe = io_data.pipe;
                    let overlapped = &mut io_data.overlapped as *mut _;
                    if let Err(e) = ConnectNamedPipe(pipe, Some(overlapped)) {
                        if e != ERROR_IO_PENDING.into() {
                            eprintln!("{:?}", e);
                        }
                    }
                }
            }
        });
    }
}

impl Drop for IocpWorkerPool {
    fn drop(&mut self) {
        let slice = unsafe {
            slice::from_raw_parts_mut::<IoData>(self.allocation as *mut _, self.pipe_instance_count)
        };
        for io_data in slice {
            let _ = unsafe { CloseHandle(io_data.pipe) };
        }

        let layout = alloc::Layout::array::<IoData>(self.pipe_instance_count)
            .expect("arguments are correct");
        unsafe { alloc::dealloc(self.allocation as *mut _, layout) };
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
struct IoData {
    overlapped: OVERLAPPED,
    pipe: HANDLE,
    buffer: [u8; BUFFER_SIZE],
    state: State,
}

impl IoData {
    fn reset(io_data: *mut Self) {
        unsafe {
            let _ = DisconnectNamedPipe((*io_data).pipe);
            (*io_data).overlapped = OVERLAPPED::default();
            (*io_data).buffer.as_mut_slice().fill(0);
            (*io_data).state = State::WaitingForConnection;
        }
    }

    unsafe fn as_usize(&mut self) -> usize {
        self as *mut _ as _
    }
}

impl Default for IoData {
    fn default() -> Self {
        IoData {
            overlapped: OVERLAPPED::default(),
            pipe: INVALID_HANDLE_VALUE,
            buffer: [0; BUFFER_SIZE],
            state: State::WaitingForConnection,
        }
    }
}

#[derive(Debug, Copy, Clone)]
enum State {
    WaitingForConnection,
    ReadEnqueued,
    WriteEnqueued,
    WaitingForDisconnect,
}

unsafe fn handle_iocp(iocp: SyncHandle, io_objects_release_channel: SyncSender<usize>) {
    let mut completion_key = 0;
    let mut bytes_transferred = 0;
    let mut overlapped = std::ptr::null_mut();

    loop {
        let result = GetQueuedCompletionStatus(
            iocp.0,
            &mut bytes_transferred,
            &mut completion_key,
            &mut overlapped,
            INFINITE,
        );

        if let Err(e) = result {
            if e == ERROR_ABANDONED_WAIT_0.into() || e == ERROR_INVALID_HANDLE.into() {
                // Thread pool is shutting down we need to quit.
                break;
            } else if e == ERROR_BROKEN_PIPE.into() && !overlapped.is_null() {
                // Client disconnected, release this `io_data` and continue.
                let io_data = overlapped as usize;
                io_objects_release_channel
                    .send(io_data)
                    .expect("pool never quits before workers");
            } else {
                // Unexpected error.
                eprintln!("{:?}", e);
            }

            continue;
        }

        let io_data = &mut *(overlapped as *mut IoData);

        match io_data.state {
            State::WaitingForConnection => {
                // Connected
                match enqueue_pipe_read(io_data) {
                    Ok(_) => io_data.state = State::ReadEnqueued,
                    Err(e) => {
                        eprintln!("{:?}", e);
                        io_objects_release_channel
                            .send(io_data.as_usize())
                            .expect("pool never quits before workers");
                    }
                }
            }
            State::ReadEnqueued => {
                let client_message: ClientMessage =
                    bincode::deserialize(&io_data.buffer[..bytes_transferred as usize]).unwrap();

                dbg!(client_message);

                let res = ServerMessage::One;
                bincode::serialize_into(&mut io_data.buffer[..], &res).unwrap();

                match enqueue_pipe_write(io_data) {
                    Ok(_) => io_data.state = State::WriteEnqueued,
                    Err(e) => {
                        eprintln!("{:?}", e);
                        io_objects_release_channel
                            .send(io_data.as_usize())
                            .expect("pool never quits before workers");
                    }
                }
            }
            State::WriteEnqueued => {
                // Enqueue dummy read so that client disconnection triggers iocp.
                match enqueue_pipe_read(io_data) {
                    Ok(_) => io_data.state = State::WaitingForDisconnect,
                    Err(_) => {
                        // If client released their handle already we get here.
                        // If not then `GetQueuedCompletionStatus` will catch this.
                        io_objects_release_channel
                            .send(io_data.as_usize())
                            .expect("pool never quits before workers");
                    }
                }
            }
            State::WaitingForDisconnect => {
                unreachable!("client disconnection will be catched before this match statement");
            }
        }
    }
}

unsafe fn enqueue_pipe_read(io_data: &mut IoData) -> windows::core::Result<()> {
    let res = ReadFile(
        io_data.pipe,
        Some(io_data.buffer.as_mut_slice()),
        None,
        Some(&mut io_data.overlapped as *mut _),
    );

    if let Err(e) = res {
        if e != ERROR_IO_PENDING.into() {
            return Err(e);
        }
    }

    Ok(())
}

unsafe fn enqueue_pipe_write(io_data: &mut IoData) -> windows::core::Result<()> {
    let res = WriteFile(
        io_data.pipe,
        Some(&mut io_data.buffer),
        None,
        Some(&mut io_data.overlapped as *mut _),
    );

    if let Err(e) = res {
        if e != ERROR_IO_PENDING.into() {
            return Err(e);
        }
    }

    Ok(())
}
