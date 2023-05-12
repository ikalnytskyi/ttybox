use std::ffi::OsString;
use std::format;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::Duration;

use base64::prelude::{Engine as _, BASE64_STANDARD};
use clap::{Args, Subcommand};
use libc;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use ncurses;

/// The controlling terminal associated with the process group of that process.
/// It can be used to write to and read from the terminal no matter how output
/// has been redirected.
const TTY_DEVICE: &str = "/dev/tty";

/// The buffer size for reading clipboard data from the terminal. One should
/// consider a trade-off between memory utilization and the frequency of system
/// calls when picking the value.
const TTY_CLIPBOARD_BUFFER_SIZE: usize = 8192;

/// The maximum waiting time for clipboard content to be pushed by the terminal
/// emulator to the terminal device. If no content has been pushed within the
/// allocated amount of time, the terminal emulator most likely doesn't support
/// OSC-52 or is simply sluggish. The value should be as small as possible to
/// provide smooth experience in unsupported terminals but remain big enough to
/// properly work in slow terminals.
const TTY_CLIPBOARD_MAX_WAIT_TIME: Duration = Duration::from_millis(500);

// const OSC_52_RESPONSE: &[u8] = Regex::new(r"\x1B]52;;[]");

#[derive(Subcommand, Debug)]
pub enum ClipboardCommands {
    Set(ClipboardSetArgs),
    Get(ClipboardGetArgs),
}

#[derive(Args, Debug)]
pub struct ClipboardSetArgs {
    /// The content to copy to clipboard.
    content: Option<OsString>,

    /// Use the "primary" clipboard.
    #[arg(short, long, default_value_t = false)]
    primary: bool,
}

#[derive(Args, Debug)]
pub struct ClipboardGetArgs {
    /// Use the "primary" clipboard.
    #[arg(short, long, default_value_t = false)]
    primary: bool,
}

pub fn execute(command: ClipboardCommands) -> io::Result<()> {
    match command {
        ClipboardCommands::Set(args) => execute_set(args),
        ClipboardCommands::Get(args) => execute_get(args),
    }
}

fn execute_set(args: ClipboardSetArgs) -> io::Result<()> {
    // If no content is supplied for copying via the command line argument it's
    // retrieved from the standard input. If the content is supplied via both
    // the command line argument and the standard input, the command line
    // argument takes precedence.
    let content = match args.content {
        Some(content) => content,
        None => OsString::from_vec(io::stdin().bytes().collect::<io::Result<Vec<_>>>()?),
    };
    osc_copy(content.as_os_str().as_bytes(), args.primary)
}

fn execute_get(args: ClipboardGetArgs) -> io::Result<()> {
    let content = osc_paste(args.primary)?;
    io::stdout().write(content.as_slice())?;
    Ok(())
}

fn osc_copy<T: AsRef<[u8]>>(content: T, primary: bool) -> io::Result<()> {
    let mut osc_copy_sequence = vec![
        b'\x1B',
        b']',
        b'5',
        b'2',
        b';',
        if primary { b'p' } else { b'c' },
        b';',
    ];
    osc_copy_sequence.extend(BASE64_STANDARD.encode(content).as_bytes());
    osc_copy_sequence.push(b'\x07');
    fs::write(TTY_DEVICE, osc_copy_sequence)?;
    Ok(())
}

// OSC 52 pasting is not as simple as copying. Aside of nuances such as
// switching terminal into noecho/cbreak mode, the procedure consists of three
// steps: (1) request paste content, (2) read and (3) decode paste response.
fn osc_paste(primary: bool) -> io::Result<Vec<u8>> {
    osc_decode_paste(
        // Switching the terminal into noecho/cbreak mode [^1] is imperative
        // before requesting the content of the clipboard. Otherwise, an OSC 52
        // paste response (escape codes + base64 encoded clipboard content) is
        // printed to the screen, and that's undesired. The response has to be
        // decoded first before being sent to the screen.
        //
        // [^1]: See `man 3 curs_inopts` for details on noecho/cbreak mode.
        with_noecho_cbreak_mode(|| {
            let mut tty = File::options().write(true).read(true).open(TTY_DEVICE)?;
            osc_request_paste(&mut tty, primary)?;
            osc_receive_paste(&mut tty)
        })?,
    )
}

fn osc_request_paste(file: &mut File, primary: bool) -> io::Result<()> {
    let osc_paste_sequence = vec![
        b'\x1B',
        b']',
        b'5',
        b'2',
        b';',
        if primary { b'p' } else { b'c' },
        b';',
        b'?',
        b'\x07',
    ];
    file.write(osc_paste_sequence.as_slice())?;
    file.flush()
}

fn osc_receive_paste(file: &mut File) -> io::Result<Vec<u8>> {
    set_nonblocking(file.as_raw_fd())?;
    read_paste_response(file)
}

// ESC] -> \x9B]
// OSC  -> \x1B
// ST   -> \x9C
// BEL  -> \x07

// FIXME: provide response example and note that the Ps can be omitted or be the same as in the
// request, and the terminating character could be different.
fn osc_decode_paste(osc_response: Vec<u8>) -> io::Result<Vec<u8>> {
    let content = osc_response
        .rsplit(|byte| *byte == b';')
        .next()
        .ok_or(io::Error::new(
            io::ErrorKind::InvalidData,
            "Cannot parse OSC 52 response.",
        ))?
        .strip_suffix(b"\x07")
        .ok_or(io::Error::new(
            io::ErrorKind::InvalidData,
            "OSC 52 response doesn't contain the terminating character.",
        ))?;
    BASE64_STANDARD.decode(content).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "OSC 52 response doesn't contain valid base64 content.",
        )
    })
}

fn with_noecho_cbreak_mode<F>(func: F) -> io::Result<Vec<u8>>
where
    F: FnOnce() -> io::Result<Vec<u8>>,
{
    ncurses::initscr();
    ncurses::noecho();
    ncurses::cbreak();

    let rv = func();

    ncurses::nocbreak();
    ncurses::echo();
    ncurses::endwin();
    return rv;
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };

    if flags < 0 {
        return Err(io::Error::last_os_error());
    }

    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_paste_response(tty: &File) -> io::Result<Vec<u8>> {
    const TOKEN: Token = Token(0);
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1);
    let mut content = Vec::<u8>::with_capacity(TTY_CLIPBOARD_BUFFER_SIZE);

    poll.registry()
        .register(&mut SourceFd(&tty.as_raw_fd()), TOKEN, Interest::READABLE)?;

    'poll: loop {
        poll.poll(&mut events, Some(TTY_CLIPBOARD_MAX_WAIT_TIME))?;

        if events.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The terminal emulator either doesn't support OSC 52 or is sluggish.",
            ));
        }

        for event in events.iter() {
            if event.token() == TOKEN && event.is_readable() {
                content.extend(read_with_draining(&tty)?);

                // TODO: Investiate whether response may end with ST character
                // instead of BEL, and add support if needed.
                if content.ends_with(b"\x07") {
                    break 'poll;
                }
            }
        }
    }
    Ok(content)
}

fn read_with_draining(mut tty: &File) -> io::Result<Vec<u8>> {
    let mut content = Vec::<u8>::with_capacity(TTY_CLIPBOARD_BUFFER_SIZE);
    let mut content_buf = [0u8; TTY_CLIPBOARD_BUFFER_SIZE];
    loop {
        match tty.read(&mut content_buf) {
            Ok(size) if size == 0 => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(size) => content.extend_from_slice(&content_buf[0..size]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(content)
}
