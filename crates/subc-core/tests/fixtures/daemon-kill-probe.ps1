param(
    [Parameter(Mandatory=$true)][string]$Root,
    [Parameter(Mandatory=$true)][int]$HarnessPid
)
$ErrorActionPreference = 'Stop'

# Keep native diagnostics in a test helper: subc-core forbids unsafe Rust.
# The observer is a sibling of the daemon, never part of the tree under test.
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class DaemonKillProbe {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct ProcessEntry {
        public uint size, usage, pid;
        public IntPtr heap;
        public uint module, threads, parent;
        public int priority;
        public uint flags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string image;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct HandleEntry {
        public IntPtr obj, pid, value;
        public uint access;
        public ushort trace, type;
        public uint attributes, reserved;
    }
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint pid);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool Process32FirstW(IntPtr snapshot, ref ProcessEntry entry);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool Process32NextW(IntPtr snapshot, ref ProcessEntry entry);
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetProcessTimes(IntPtr process, out long created, out long exited, out long kernel, out long user);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool QueryFullProcessImageNameW(IntPtr process, uint flags, StringBuilder image, ref int size);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool IsProcessInJob(IntPtr process, IntPtr job, out bool result);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool DuplicateHandle(IntPtr source, IntPtr handle, IntPtr target, out IntPtr copy, uint access, bool inherit, uint options);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern IntPtr CreateJobObjectW(IntPtr security, string name);
    [DllImport("ntdll.dll")] static extern int NtQuerySystemInformation(int info, IntPtr buffer, int size, out int needed);

    sealed class Tracked {
        public uint pid, parent;
        public long created, exitObserved;
        public string image;
        public IntPtr handle;
        public bool exited;
    }
    static string root, log;
    static readonly Stopwatch clock = new Stopwatch();
    static readonly Dictionary<uint, Tracked> tree = new Dictionary<uint, Tracked>();
    static readonly HashSet<long> daemonObjects = new HashSet<long>();
    static ushort jobType, fileType;
    static uint daemonPid;

    static void Line(string text) {
        File.AppendAllText(log, text + "\n");
    }
    static string Hex(IntPtr value) { return "0x" + value.ToInt64().ToString("x"); }
    static long Created(IntPtr handle) {
        long created, exited, kernel, user;
        if (!GetProcessTimes(handle, out created, out exited, out kernel, out user))
            throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        return created;
    }
    static string Membership(IntPtr process, IntPtr job) {
        bool member;
        if (!IsProcessInJob(process, job, out member))
            return "error:" + Marshal.GetLastWin32Error();
        return member ? "true" : "false";
    }
    static Dictionary<uint, ProcessEntry> Snapshot() {
        IntPtr snapshot = CreateToolhelp32Snapshot(2, 0);
        if (snapshot == new IntPtr(-1)) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        try {
            var entries = new Dictionary<uint, ProcessEntry>();
            var entry = new ProcessEntry();
            entry.size = (uint)Marshal.SizeOf(typeof(ProcessEntry));
            if (!Process32FirstW(snapshot, ref entry)) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            do { entries[entry.pid] = entry; } while (Process32NextW(snapshot, ref entry));
            return entries;
        } finally { CloseHandle(snapshot); }
    }
    static Tracked Open(ProcessEntry entry) {
        IntPtr handle = OpenProcess(0x1000 | 0x100000 | 1, false, entry.pid);
        if (handle == IntPtr.Zero) {
            Line("open-error|" + clock.ElapsedMilliseconds + "|" + entry.pid + "|" + Marshal.GetLastWin32Error());
            return null;
        }
        try {
            long created = Created(handle);
            var image = new StringBuilder(32768);
            int size = image.Capacity;
            string path = QueryFullProcessImageNameW(handle, 0, image, ref size) ? image.ToString() : entry.image;
            return new Tracked { pid = entry.pid, parent = entry.parent, created = created, image = path, handle = handle };
        } catch { CloseHandle(handle); throw; }
    }
    static void Add(Tracked process) {
        tree.Add(process.pid, process);
        Line("seen|" + clock.ElapsedMilliseconds + "|" + process.pid + "|" + process.parent + "|" + process.created + "|" + Membership(process.handle, IntPtr.Zero) + "|" + process.image);
    }
    static void Sample() {
        var entries = Snapshot();
        bool added;
        do {
            added = false;
            foreach (var entry in entries.Values) {
                Tracked parent;
                if (tree.ContainsKey(entry.pid) || !tree.TryGetValue(entry.parent, out parent)) continue;
                var child = Open(entry);
                if (child == null) continue;
                // The parent's PID alone is not an identity: reject a reused PID's
                // later children after the original parent's recorded exit.
                if (child.created < parent.created || (parent.exited && child.created > parent.exitObserved)) {
                    CloseHandle(child.handle);
                    continue;
                }
                Add(child);
                added = true;
            }
        } while (added);
        foreach (var process in tree.Values) {
            if (!process.exited && WaitForSingleObject(process.handle, 0) == 0) {
                process.exited = true;
                process.exitObserved = DateTime.UtcNow.ToFileTimeUtc();
                Line("exit|" + clock.ElapsedMilliseconds + "|" + process.pid + "|" + process.created);
            }
        }
    }
    static List<HandleEntry> Handles() {
        int size = 1024 * 1024;
        while (size <= 256 * 1024 * 1024) {
            IntPtr buffer = Marshal.AllocHGlobal(size);
            try {
                int needed;
                int status = NtQuerySystemInformation(64, buffer, size, out needed);
                if (status == unchecked((int)0xc0000004)) {
                    size = Math.Max(size * 2, needed + 65536);
                    continue;
                }
                if (status < 0) throw new Exception("NtQuerySystemInformation: 0x" + status.ToString("x"));
                long count = Marshal.ReadIntPtr(buffer).ToInt64();
                int stride = Marshal.SizeOf(typeof(HandleEntry));
                if (count < 0 || count > (size - 2 * IntPtr.Size) / stride) throw new Exception("Invalid handle table size");
                var handles = new List<HandleEntry>();
                for (int i = 0; i < count; i++) {
                    var ptr = IntPtr.Add(buffer, 2 * IntPtr.Size + i * stride);
                    handles.Add((HandleEntry)Marshal.PtrToStructure(ptr, typeof(HandleEntry)));
                }
                return handles;
            } finally { Marshal.FreeHGlobal(buffer); }
        }
        throw new Exception("Handle table exceeds 256 MiB");
    }
    static void Dump(string phase, int harnessPid, bool checkDaemonJobs) {
        List<HandleEntry> handles;
        try { handles = Handles(); }
        catch (Exception error) { Line("handle-error|" + phase + "|" + error.Message); return; }
        var owners = new HashSet<long>();
        foreach (var process in tree.Values) {
            if (WaitForSingleObject(process.handle, 0) != 0) owners.Add(process.pid);
        }
        foreach (var entry in handles) {
            if (entry.pid.ToInt64() == daemonPid && entry.obj != IntPtr.Zero
                && (entry.type == jobType || entry.type == fileType))
                daemonObjects.Add(entry.obj.ToInt64());
        }
        if (!checkDaemonJobs) {
            foreach (var entry in handles) {
                bool related = !owners.Contains(entry.pid.ToInt64());
                if (related && !daemonObjects.Contains(entry.obj.ToInt64())) continue;
                // Object addresses correlate inherited/duplicated handles across
                // processes without querying pipe names, which can block forever.
                Line("handle|" + phase + "|" + entry.pid + "|value=" + Hex(entry.value) + "|object=" + Hex(entry.obj) + "|type=" + (entry.type == jobType ? "Job" : entry.type == fileType ? "File" : entry.type.ToString()) + "|access=0x" + entry.access.ToString("x") + "|attributes=0x" + entry.attributes.ToString("x") + "|related_owner=" + related + "|harness_owner=" + (entry.pid.ToInt64() == harnessPid));
            }
            return;
        }
        // Only the small membership report is written before the timed kill;
        // per-handle logging would perturb the teardown interval being measured.
        IntPtr daemon = OpenProcess(0x40, false, daemonPid);
        if (daemon == IntPtr.Zero) { Line("job-probe-error|" + Marshal.GetLastWin32Error()); return; }
        try {
            int jobs = 0;
            foreach (var entry in handles) {
                if (entry.pid.ToInt64() != daemonPid || entry.type != jobType) continue;
                jobs++;
                IntPtr job;
                if (!DuplicateHandle(daemon, entry.value, GetCurrentProcess(), out job, 0, false, 2)) {
                    Line("job-duplicate-error|" + Marshal.GetLastWin32Error());
                    continue;
                }
                try {
                    foreach (var process in tree.Values) {
                        if (!process.exited)
                            Line("daemon-job|" + phase + "|" + process.pid + "|" + process.created + "|object=" + Hex(entry.obj) + "|member=" + Membership(process.handle, job));
                    }
                } finally { CloseHandle(job); }
            }
            Line("daemon-job-count|" + phase + "|" + jobs);
        } finally { CloseHandle(daemon); }
        // The ready marker is written only after all duplicated job handles close.
        // A diagnostic handle must not keep the job alive during the hard kill.
    }

    public static void Run(string directory, int harnessPid) {
        root = directory;
        log = Path.Combine(root, "probe.log");
        clock.Start();
        IntPtr probeJob = CreateJobObjectW(IntPtr.Zero, null);
        if (probeJob == IntPtr.Zero) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        try {
            using (var probeFile = new FileStream(log, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.ReadWrite)) {
                IntPtr file = probeFile.SafeFileHandle.DangerousGetHandle();
                foreach (var entry in Handles()) {
                    if (entry.pid.ToInt64() != Process.GetCurrentProcess().Id) continue;
                    if (entry.value == probeJob) jobType = entry.type;
                    if (entry.value == file) fileType = entry.type;
                }
            }
            if (jobType == 0 || fileType == 0) throw new Exception("Could not identify Job/File handle types");
        } finally { CloseHandle(probeJob); }
        File.WriteAllText(Path.Combine(root, "observer.ready"), "ready");
        try {
            while (!File.Exists(Path.Combine(root, "daemon.pid"))) {
                if (clock.ElapsedMilliseconds > 15000) throw new Exception("Daemon PID not published");
                Thread.Sleep(10);
            }
            // Publish the PID atomically in the Rust harness so no partial number
            // is interpreted as a different process.
            daemonPid = UInt32.Parse(File.ReadAllText(Path.Combine(root, "daemon.pid")).Trim());
            var entries = Snapshot();
            if (!entries.ContainsKey(daemonPid)) throw new Exception("Daemon gone before observation");
            var daemon = Open(entries[daemonPid]);
            if (daemon == null) throw new Exception("Cannot open daemon");
            Add(daemon);
            bool prepared = false;
            long killedAt = -1;
            while (clock.ElapsedMilliseconds < 45000) {
                Sample();
                if (!prepared && File.Exists(Path.Combine(root, "prepare"))) {
                    Dump("before-kill", harnessPid, true);
                    prepared = true;
                    File.WriteAllText(Path.Combine(root, "prepared"), "ready");
                }
                if (File.Exists(Path.Combine(root, "killed")) && killedAt < 0) killedAt = clock.ElapsedMilliseconds;
                if (killedAt >= 0 && clock.ElapsedMilliseconds - killedAt >= 5000) {
                    int survivors = 0;
                    foreach (var process in tree.Values) {
                        if (WaitForSingleObject(process.handle, 0) != 0) {
                            survivors++;
                            Line("survivor|" + process.pid + "|" + process.parent + "|" + process.created + "|in_job=" + Membership(process.handle, IntPtr.Zero) + "|" + process.image);
                        }
                    }
                    if (survivors != 0) Dump("survivor", harnessPid, false);
                    Line("result|survivors=" + survivors + "|identities=" + tree.Count);
                    File.WriteAllText(Path.Combine(root, "probe.done"), survivors.ToString());
                    return;
                }
                Thread.Sleep(10);
            }
            throw new Exception("Observation exceeded 45 seconds");
        } finally {
            // Retained process handles identify the original instances even if
            // Windows has since recycled their PIDs. Cleanup is after evidence.
            foreach (var process in tree.Values) {
                if (WaitForSingleObject(process.handle, 0) != 0) TerminateProcess(process.handle, 99);
                CloseHandle(process.handle);
            }
        }
    }
}
'@
[DaemonKillProbe]::Run($Root, $HarnessPid)
