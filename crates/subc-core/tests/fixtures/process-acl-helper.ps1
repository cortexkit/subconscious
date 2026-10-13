param(
    [string] $Daemon,
    [string] $LaunchRecord,
    [uint32] $TargetPid,
    [uint64] $TargetStartTime,
    [switch] $WaitGone
)
$ErrorActionPreference = 'Stop'

# Keep native calls in a disposable helper: removing a privilege from the test
# runner would affect unrelated tests, and core deliberately forbids unsafe Rust.
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text;

public static class ProcessAclProbe {
    [StructLayout(LayoutKind.Sequential)]
    struct Luid { public uint Low; public int High; }
    [StructLayout(LayoutKind.Sequential)]
    struct Privilege { public Luid Id; public uint Attributes; }
    [StructLayout(LayoutKind.Sequential)]
    struct OnePrivilege { public uint Count; public Privilege Entry; }
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")] static extern void SetLastError(uint error);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [StructLayout(LayoutKind.Sequential)]
    struct FileTime { public uint Low; public uint High; }
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetProcessTimes(IntPtr process, out FileTime creation, out FileTime exit,
        out FileTime kernel, out FileTime user);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint WaitForSingleObject(IntPtr process, uint milliseconds);

    static ulong CreationTime(IntPtr process) {
        FileTime creation, exit, kernel, user;
        if (!GetProcessTimes(process, out creation, out exit, out kernel, out user))
            throw new Win32Exception(Marshal.GetLastWin32Error(), "GetProcessTimes");
        return ((ulong)creation.High << 32) | creation.Low;
    }

    public static void WaitGone(uint pid, ulong expected) {
        IntPtr process = OpenProcess(0x101001, false, pid);
        if (process == IntPtr.Zero) {
            int error = Marshal.GetLastWin32Error();
            if (error == 87) return; // ERROR_INVALID_PARAMETER: no process with this PID.
            throw new Win32Exception(error, "OpenProcess(wait)");
        }
        try {
            if (CreationTime(process) != expected) return; // PID reused: original process is gone.
            uint result = WaitForSingleObject(process, 60000);
            if (result != 0) throw new Exception("process survived bounded wait: " + result);
        } finally { CloseHandle(process); }
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder image, ref uint size);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool LookupPrivilegeValue(string system, string name, out Luid id);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetTokenInformation(IntPtr token, int kind, IntPtr data, uint size, out uint needed);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool AdjustTokenPrivileges(IntPtr token, bool disableAll, ref OnePrivilege privileges,
        uint size, IntPtr previous, IntPtr needed);

    static bool HasDebug(IntPtr token, Luid debug) {
        uint size;
        GetTokenInformation(token, 3, IntPtr.Zero, 0, out size);
        int error = Marshal.GetLastWin32Error();
        if (size == 0 || error != 122) throw new Win32Exception(error, "TokenPrivileges size");
        IntPtr data = Marshal.AllocHGlobal(checked((int)size));
        try {
            if (!GetTokenInformation(token, 3, data, size, out size))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "TokenPrivileges");
            int count = Marshal.ReadInt32(data);
            int stride = Marshal.SizeOf(typeof(Privilege));
            for (int i = 0; i < count; i++) {
                Privilege privilege = (Privilege)Marshal.PtrToStructure(
                    IntPtr.Add(data, 4 + i * stride), typeof(Privilege));
                if (privilege.Id.Low == debug.Low && privilege.Id.High == debug.High) return true;
            }
            return false;
        } finally { Marshal.FreeHGlobal(data); }
    }

    public static string RemoveDebug() {
        IntPtr token;
        if (!OpenProcessToken(GetCurrentProcess(), 0x28, out token))
            throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenProcessToken(adjust/query)");
        try {
            Luid debug;
            if (!LookupPrivilegeValue(null, "SeDebugPrivilege", out debug))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "LookupPrivilegeValue");
            OnePrivilege privileges = new OnePrivilege();
            privileges.Count = HasDebug(token, debug) ? 1u : 0u;
            privileges.Entry.Id = debug;
            privileges.Entry.Attributes = 4; // SE_PRIVILEGE_REMOVED, not merely disabled.
            SetLastError(0);
            bool adjusted = AdjustTokenPrivileges(token, false, ref privileges, 0, IntPtr.Zero, IntPtr.Zero);
            int error = Marshal.GetLastWin32Error();
            if (!adjusted || error != 0) throw new Win32Exception(error, "AdjustTokenPrivileges(remove)");
            if (HasDebug(token, debug)) throw new Exception("SeDebugPrivilege still present");
            return "SeDebugPrivilege=absent";
        } finally { CloseHandle(token); }
    }

    public static string Probe(uint pid) {
        // The successful narrow open rules out a dead PID or a denial unrelated
        // to VM_READ, and also supplies the token for the same-user check.
        IntPtr process = OpenProcess(0x101001, false, pid);
        if (process == IntPtr.Zero)
            throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenProcess(query/synchronize/terminate)");
        string image;
        ulong startTime;
        try {
            startTime = CreationTime(process);
            StringBuilder buffer = new StringBuilder(32768);
            uint size = (uint)buffer.Capacity;
            if (!QueryFullProcessImageName(process, 0, buffer, ref size))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "QueryFullProcessImageName");
            image = buffer.ToString();
            IntPtr token;
            if (!OpenProcessToken(process, 8, out token))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenProcessToken(target query)");
            try {
                using (WindowsIdentity target = new WindowsIdentity(token))
                using (WindowsIdentity caller = WindowsIdentity.GetCurrent()) {
                    if (target.User.Value != caller.User.Value) throw new Exception("target is not the same user");
                }
            } finally { CloseHandle(token); }
        } finally { CloseHandle(process); }
        IntPtr denied = OpenProcess(0x10, false, pid); // PROCESS_VM_READ
        int error = Marshal.GetLastWin32Error();
        if (denied != IntPtr.Zero) {
            CloseHandle(denied);
            throw new Exception("OpenProcess(PROCESS_VM_READ) unexpectedly succeeded");
        }
        if (error != 5) throw new Win32Exception(error, "expected ERROR_ACCESS_DENIED for PROCESS_VM_READ");
        return "same-user; narrow-open=ok; PROCESS_VM_READ=ERROR_ACCESS_DENIED(5); image=" + image
            + "\ncreation-time=" + startTime;
    }
}
'@

$privileges = [ProcessAclProbe]::RemoveDebug()
if ($Daemon) {
    # Strip the launcher too: an elevated daemon would bypass the module DACL
    # and conceal a regression that asks OpenProcess for excessive rights.
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $Daemon
    $start.UseShellExecute = $false
    $process = [System.Diagnostics.Process]::Start($start)
    [System.IO.File]::WriteAllText($LaunchRecord, ($process.Id.ToString() + ';' + $privileges))
    $process.WaitForExit()
    exit $process.ExitCode
} elseif ($WaitGone) {
    [ProcessAclProbe]::WaitGone($TargetPid, $TargetStartTime)
    Write-Output 'exit=confirmed'
} else {
    Write-Output $privileges
    Write-Output ([ProcessAclProbe]::Probe($TargetPid))
}
