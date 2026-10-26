use std::{
    io::{Read, Write},
    process::Command,
};

// A real terminal is needed here: setting a width or a color variable alone
// would never exercise stdout's terminal/pipe dispatch boundary.
pub fn screen(command: &Command) -> vt100::Parser {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut builder = CommandBuilder::new(command.get_program());
    builder.args(command.get_args());
    builder.env("TERM", "xterm-256color");
    builder.env("CK_THEME", "light");
    builder.env_remove("NO_COLOR");
    for (name, value) in command.get_envs() {
        if let Some(value) = value {
            builder.env(name, value);
        } else {
            builder.env_remove(name);
        }
    }
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let read = std::thread::spawn(move || {
        // On Windows, portable-pty creates the pseudoconsole with
        // PSEUDOCONSOLE_INHERIT_CURSOR, so conhost first asks the hosting
        // terminal for the cursor position (ESC [ 6 n) and holds the child's
        // output until it gets an answer. Answer once, as a terminal would.
        // Unix terminals never send the request.
        const REQUEST: &[u8] = b"\x1b[6n";
        let mut bytes = Vec::new();
        let mut answered = false;
        let mut chunk = [0u8; 4096];
        loop {
            let n = match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            bytes.extend_from_slice(&chunk[..n]);
            if !answered && bytes.windows(REQUEST.len()).any(|w| w == REQUEST) {
                answered = true;
                let _ = writer.write_all(b"\x1b[1;1R");
                let _ = writer.flush();
            }
        }
        bytes
    });
    let mut child = pair.slave.spawn_command(builder).unwrap();
    drop(pair.slave);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("terminal command did not exit within 15 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(status.exit_code(), 0);
    drop(pair.master);
    let bytes = read.join().unwrap();
    let mut parser = vt100::Parser::new(30, 100, 0);
    parser.process(&bytes);
    parser
}
