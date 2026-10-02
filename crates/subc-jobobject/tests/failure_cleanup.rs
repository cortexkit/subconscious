#[path = "../src/cleanup.rs"]
mod cleanup;

#[test]
fn failed_containment_kills_the_owned_child_on_every_early_return() {
    use std::{cell::Cell, io};
    for failure in ["missing process handle", "job assignment", "thread resume"] {
        let killed = Cell::new(false);
        let result = cleanup::start(
            &killed,
            |_| Err(io::Error::other(failure)),
            |child| {
                child.set(true);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(killed.get(), "{failure} leaked a suspended child");
    }
    let killed = Cell::new(false);
    let result = cleanup::start(
        &killed,
        |_| Ok(()),
        |child| {
            child.set(true);
            Ok(())
        },
    );
    assert!(result.is_ok());
    assert!(
        !killed.get(),
        "successful containment must retain the child"
    );
}
