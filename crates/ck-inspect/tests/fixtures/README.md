# Inspection fixtures

`sample.txt` is the call graph from `/usr/bin/sample <pid> 1 10 -file <output>`
against `sample-helper.rs`, compiled with `rustc -g`. It contains a sleeping main
thread and a busy named worker. Only the call graph is retained: process headers,
binary image paths, and machine metadata are excluded. It is not a module sample.

`proc/start` and `proc/end` model Linux task directories at opposite ends of a
window. They include spaces and a closing parenthesis in a command name, an idle
thread, and a reused thread id whose start time changes.
