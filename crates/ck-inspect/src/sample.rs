use crate::profile::{Profile, Stack, Thread};

struct Node {
    depth: usize,
    count: u64,
    children: u64,
    frame: String,
}

fn frame_name(text: &str) -> String {
    // A stripped binary gives `???  (in image)  load address A + 0xOFF  [..]`.
    // Keep the image and its offset, so the owner can symbolize the frame
    // against the build's own debug symbols, instead of reporting a bare `???`.
    if text.starts_with("???") {
        if let Some(image) = text
            .split_once("(in ")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(image, _)| image.trim())
        {
            let offset = text
                .split_once("load address ")
                .and_then(|(_, rest)| rest.split_once(" + "))
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .filter(|o| o.starts_with("0x") && o[2..].chars().all(|c| c.is_ascii_hexdigit()));
            return match offset {
                Some(offset) => format!("{image}+{offset}"),
                None => format!("??? ({image})"),
            };
        }
    }
    let text = text.split("  (in ").next().unwrap_or(text);
    let text = text.split(" + ").next().unwrap_or(text);
    let text = text.split("  [").next().unwrap_or(text).trim();
    if text.starts_with("0x") && text[2..].chars().all(|c| c.is_ascii_hexdigit()) {
        return "<unknown>".into();
    }
    match rustc_demangle::try_demangle(text) {
        Ok(symbol) => format!("{symbol:#}"),
        Err(_) => text.to_string(),
    }
}

// Leaf frames where a thread is blocked in the kernel rather than running:
// condition variables, mutexes and futex-style locks, event queues, Mach
// messages, semaphores, sleeps, and blocking I/O waits.
fn waiting(frame: &str) -> bool {
    [
        "__psynch_cvwait",
        "__psynch_mutexwait",
        "__ulock_wait",
        "__ulock_wait2",
        "kevent",
        "kevent64",
        "kevent_id",
        "mach_msg",
        "mach_msg2_trap",
        "__semwait_signal",
        "semaphore_wait_trap",
        "semaphore_timedwait_trap",
        "__workq_kernreturn",
        "read",
        "__read_nocancel",
        "__select",
        "poll",
        "__poll_nocancel",
        "__sigsuspend",
        "__wait4",
    ]
    .iter()
    .any(|wait| frame == *wait || frame.starts_with(&format!("{wait}_")))
}

fn finish_node(path: &mut Vec<Node>, stacks: &mut Vec<Stack>) -> bool {
    let node = path.pop().expect("nonempty path");
    // Inclusive counts need subtraction: samples ending in a parent are not
    // represented by its children and would otherwise disappear from the profile.
    let own = node.count.saturating_sub(node.children);
    if own > 0 {
        let frames = std::iter::once(node.frame)
            .chain(path.iter().rev().map(|n| n.frame.clone()))
            .take(12)
            .collect();
        stacks.push(Stack::new(own, frames));
    }
    own > 0 && path.len() >= 12
}

// `cut` collects frame-depth truncation for the thread being finished. It only
// marks the reply truncated if that thread is reported: an idle thread is
// dropped whole, so cutting its deep wait stacks removes nothing from the reply.
fn finish_thread(
    profile: &mut Profile,
    thread: &mut Option<Thread>,
    path: &mut Vec<Node>,
    cut: &mut bool,
) {
    let Some(mut thread) = thread.take() else {
        *cut = false;
        return;
    };
    while !path.is_empty() {
        *cut |= finish_node(path, &mut thread.stacks);
    }
    let thread_cut = std::mem::take(cut);
    // Only sampled execution positions determine idleness. A waiting leaf has
    // ordinary runtime ancestors; an active leaf in any branch keeps the thread,
    // and only its active stacks are reported, so waits never take the place
    // of the code that was running.
    thread
        .stacks
        .retain(|s| !s.frames.first().is_some_and(|f| waiting(f)));
    let busy: u64 = thread.stacks.iter().map(|s| s.count).sum();
    if busy == 0 {
        profile.idle_threads += 1;
        return;
    }
    thread.busy_samples = Some(busy);
    profile.truncated |= thread_cut;
    // Different instruction offsets can resolve to the same named stack. Merge
    // them before ranking so the reply does not repeat one stack three times.
    let mut counts = std::collections::BTreeMap::<Vec<String>, u64>::new();
    for stack in thread.stacks.drain(..) {
        *counts.entry(stack.frames).or_default() += stack.count;
    }
    thread.stacks = counts
        .into_iter()
        .map(|(frames, count)| Stack::new(count, frames))
        .collect();
    thread
        .stacks
        .sort_by(|a, b| b.count.cmp(&a.count).then(a.frames.cmp(&b.frames)));
    profile.truncated |= thread.stacks.len() > 3;
    thread.stacks.truncate(3);
    profile.threads.push(thread);
}

pub(crate) fn parse(text: &str, mut profile: Profile) -> Result<Profile, &'static str> {
    let mut in_graph = false;
    let mut thread = None;
    let mut path: Vec<Node> = Vec::new();
    let mut cut = false;
    for line in text.lines() {
        if line.trim() == "Call graph:" {
            in_graph = true;
            continue;
        }
        if !in_graph {
            continue;
        }
        if line.starts_with("Total number") || line.starts_with("Binary Images:") {
            break;
        }
        let Some(start) = line.find(|c: char| c.is_ascii_digit()) else {
            continue;
        };
        let rest = &line[start..];
        let Some((count, label)) = rest.split_once(' ') else {
            continue;
        };
        let Ok(count) = count.parse::<u64>() else {
            continue;
        };
        let label = label.trim();
        if label.starts_with("Thread_") {
            finish_thread(&mut profile, &mut thread, &mut path, &mut cut);
            let mut next = Thread::new(label.into());
            next.samples = Some(count);
            thread = Some(next);
        } else if let Some(thread) = thread.as_mut() {
            while path.last().is_some_and(|n| n.depth >= start) {
                cut |= finish_node(&mut path, &mut thread.stacks);
            }
            if let Some(parent) = path.last_mut() {
                parent.children = parent.children.saturating_add(count);
            }
            let frame = frame_name(label);
            path.push(Node {
                depth: start,
                count,
                children: 0,
                frame,
            });
        }
    }
    finish_thread(&mut profile, &mut thread, &mut path, &mut cut);
    if profile.threads.is_empty() && profile.idle_threads == 0 {
        return Err("sample contained no call graph");
    }
    // `sample` samples every thread at every tick, so total samples are equal
    // across threads. Rank by samples spent running instead.
    profile.threads.sort_by(|a, b| {
        b.busy_samples
            .cmp(&a.busy_samples)
            .then(a.name.cmp(&b.name))
    });
    profile.truncated |= profile.threads.len() > 8;
    profile.threads.truncate(8);
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> Profile {
        Profile::new("fixture".into(), 1, 1, "sample".into())
    }

    #[test]
    fn generated_helper_graph_demangles_and_ranks_exclusive_stacks() {
        let result = parse(include_str!("../tests/fixtures/sample.txt"), profile()).unwrap();
        assert_eq!(result.idle_threads, 1);
        assert_eq!(result.threads.len(), 1);
        let t = &result.threads[0];
        assert_eq!(t.name, "Thread_319321230: profile-worker");
        assert_eq!(t.samples, Some(90));
        assert_eq!(
            t.stacks.iter().map(|s| s.count).collect::<Vec<_>>(),
            [31, 19, 14]
        );
        assert!(t.stacks.iter().all(|s| s.frames.len() <= 12));
        assert!(t.stacks[0].frames.iter().any(|f| f.contains("core::")));
        for s in &t.stacks {
            for f in &s.frames {
                assert!(!f.contains("0x") && !f.contains("(in ") && !f.contains(" + "));
                assert!(!f.starts_with("_R"));
            }
        }
    }

    #[test]
    fn legacy_and_v0_frames_have_no_hash_or_offsets() {
        assert_eq!(frame_name("0xabcdef"), "<unknown>");
        assert_eq!(
            frame_name("_ZN4test4main17h0123456789abcdefE  (in helper) + 4 [0x123]"),
            "test::main"
        );
        assert_eq!(
            frame_name("_RNvC6_123foo3bar  (in helper) + 8 [0x456]"),
            "123foo::bar"
        );
    }

    #[test]
    fn deep_stack_is_leaf_first_bounded_and_marked_truncated() {
        let mut graph = "Call graph:\n  1 Thread_1\n".to_string();
        for depth in 0..15 {
            graph.push_str(&format!("{}1 frame_{depth}\n", " ".repeat(4 + depth * 2)));
        }
        let result = parse(&graph, profile()).unwrap();
        assert!(result.truncated);
        let stack = &result.threads[0].stacks[0];
        assert_eq!(stack.frames.len(), 12);
        assert_eq!(stack.frames[0], "frame_14");
        assert_eq!(stack.frames[11], "frame_3");
    }

    #[test]
    fn an_idle_thread_with_a_deep_wait_stack_does_not_mark_the_reply_truncated() {
        let mut graph = "Call graph:\n  1 Thread_1\n".to_string();
        for depth in 0..14 {
            graph.push_str(&format!("{}1 frame_{depth}\n", " ".repeat(4 + depth * 2)));
        }
        graph.push_str(&format!("{}1 __psynch_cvwait\n", " ".repeat(4 + 14 * 2)));
        let result = parse(&graph, profile()).unwrap();
        assert_eq!(result.idle_threads, 1);
        assert!(result.threads.is_empty());
        assert!(!result.truncated);
    }

    #[test]
    fn mixed_wait_and_work_is_not_idle_and_threads_are_bounded() {
        let mut graph = "Call graph:\n".to_string();
        for i in 1..=10 {
            graph.push_str(&format!(
                "  {i} Thread_{i}\n    {i} root\n      1 kevent\n      {} work\n",
                i - 1
            ));
        }
        let result = parse(&graph, profile()).unwrap();
        assert_eq!(result.idle_threads, 1);
        assert_eq!(result.threads.len(), 8);
        assert_eq!(result.threads[0].samples, Some(10));
        assert!(result.truncated);
        assert_eq!(result.threads[0].busy_samples, Some(9));
        assert!(result.threads[0]
            .stacks
            .iter()
            .all(|s| s.frames[0] != "kevent"));
        assert!(parse("garbage", profile()).is_err());
    }

    #[test]
    fn threads_rank_by_busy_samples_not_by_equal_totals_or_name() {
        // `sample` reports the same total for every thread. A mostly waiting
        // thread whose name sorts first must not push the busy one out.
        let graph = "Call graph:\n\
            \x20 100 Thread_1: aaa-mostly-waiting\n\
            \x20   100 root\n\
            \x20     98 __psynch_cvwait\n\
            \x20     2 tick\n\
            \x20 100 Thread_2: zzz-hot\n\
            \x20   100 root\n\
            \x20     95 hot_loop\n\
            \x20     5 kevent\n";
        let result = parse(graph, profile()).unwrap();
        let ranked: Vec<_> = result
            .threads
            .iter()
            .map(|t| (t.name.as_str(), t.busy_samples))
            .collect();
        assert_eq!(
            ranked,
            [
                ("Thread_2: zzz-hot", Some(95)),
                ("Thread_1: aaa-mostly-waiting", Some(2)),
            ]
        );
        assert_eq!(result.threads[0].stacks[0].frames[0], "hot_loop");
    }

    #[test]
    fn blocking_reads_locks_and_mach_traps_are_waits() {
        let mut graph = "Call graph:\n".to_string();
        for (i, leaf) in [
            "read",
            "__ulock_wait2",
            "mach_msg2_trap",
            "__psynch_mutexwait",
        ]
        .iter()
        .enumerate()
        {
            graph.push_str(&format!("  10 Thread_{i}\n    10 root\n      10 {leaf}\n"));
        }
        let result = parse(&graph, profile()).unwrap();
        assert_eq!(result.idle_threads, 4);
        assert!(result.threads.is_empty());
    }

    #[test]
    fn stripped_frames_keep_their_image_and_offset() {
        assert_eq!(
            frame_name("???  (in ck-aft)  load address 0x100f8c000 + 0x4dd5f4  [0x1014695f4]"),
            "ck-aft+0x4dd5f4"
        );
        assert_eq!(frame_name("???  (in ck-aft)"), "??? (ck-aft)");
    }
}
