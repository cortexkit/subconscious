// A stand-in for the daemon's spawn: it is started with the launch-nonce pipe
// already at descriptor 3, names that pipe in SUBC_LAUNCH_NONCE_FD by its
// inode, and spawns the command in argv with the pipe at descriptor 3 again.
// The inode is only known once the pipe exists, which is why a shell cannot
// set the variable itself.
//
// LAUNCHER_FD_VALUE overrides the variable's value; "{ino}" in it stands for
// the real inode, so a test can name the wrong descriptor or the wrong pipe.
import { spawnSync } from "node:child_process";
import { fstatSync } from "node:fs";

const inode = String(BigInt.asUintN(64, fstatSync(3, { bigint: true }).ino));
const fdValue = (process.env.LAUNCHER_FD_VALUE ?? "3:{ino}").replace("{ino}", inode);
const env = { ...process.env, SUBC_LAUNCH_NONCE_FD: fdValue };
delete env.LAUNCHER_FD_VALUE;

const [command, ...args] = process.argv.slice(2);
const result = spawnSync(command, args, {
  stdio: ["ignore", "pipe", "inherit", 3],
  env,
  encoding: "utf8",
});
process.stdout.write(result.stdout ?? "");
process.exit(result.status ?? 1);
