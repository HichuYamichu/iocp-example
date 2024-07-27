use iocp_shared::*;
use windows::core::{s, PCSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Pipes::*;
use windows::Win32::System::Threading::INFINITE;

const PIPE_NAME: PCSTR = s!("\\\\.\\pipe\\iocp_example_pipe");
const BUFFER_SIZE: usize = 512;

fn main() -> windows::core::Result<()> {
    unsafe { WaitNamedPipeA(PIPE_NAME, INFINITE) }?;
    let pipe = unsafe {
        CreateFileA(
            PIPE_NAME,
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )?
    };

    let client_message = ClientMessage::A;
    let mut buffer = [0u8; BUFFER_SIZE];
    bincode::serialize_into(buffer.as_mut_slice(), &client_message).unwrap();
    let mut bytes_written = 0;

    unsafe {
        WriteFile(
            pipe,
            Some(buffer.as_slice()),
            Some(&mut bytes_written),
            None,
        )?;
    }

    let mut bytes_read = 0;
    unsafe {
        ReadFile(pipe, Some(&mut buffer), Some(&mut bytes_read), None)?;
    };

    let server_message: ServerMessage =
        bincode::deserialize(&buffer[..bytes_read as usize]).unwrap();

    dbg!(server_message);

    unsafe { CloseHandle(pipe)? };

    Ok(())
}
