//! Exec target for admission-gate tests; never used as a real module.

#[cfg(unix)]
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 3);
    for descriptor in args[2].split(',') {
        let fields: Vec<_> = descriptor.split(':').collect();
        let fd = fields[0].parse::<i32>().unwrap();
        let device = fields[1].parse::<u64>().unwrap();
        let inode = fields[2].parse::<u64>().unwrap();
        // SAFETY: fstat writes only the initialized structure provided here.
        #[allow(unsafe_code)]
        let inherited = unsafe {
            let mut facts: libc::stat = std::mem::zeroed();
            libc::fstat(fd, &mut facts) == 0
                && facts.st_dev as u64 == device
                && facts.st_ino as u64 == inode
        };
        assert!(!inherited, "module inherited admission descriptor {fd}");
    }
    std::fs::write(&args[1], "exec\n").unwrap();
}

#[cfg(not(unix))]
fn main() {}
