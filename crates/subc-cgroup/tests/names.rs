// The directory encoder is portable; exercise the same implementation on every host.
#[path = "../src/name.rs"]
mod name;

#[test]
fn cgroup_names_are_injective_and_do_not_name_kernel_interfaces() {
    use name::module_directory_name as encode;
    for (left, right) in [("x y", "x_20y"), ("a:b", "a_3ab"), ("é", "_c3_a9")] {
        assert_ne!(encode(left), encode(right), "{left:?} aliases {right:?}");
    }
    for id in ["cgroup.procs", "cgroup.kill", "memory.max", ".", "..", ""] {
        let name = encode(id);
        assert!(
            name.starts_with("m-"),
            "module directories must occupy their own namespace: {name}"
        );
        assert!(!name.contains('/'));
    }
}
