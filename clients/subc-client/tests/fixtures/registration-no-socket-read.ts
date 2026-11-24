import fs from "node:fs";

// Make a return to waiting for socket EOF fail even if shutdown would wake it.
fs.readSync = new Proxy(fs.readSync, {
  apply(read, receiver, args) {
    if (args[0] === 4) throw new Error("legacy fixture must not wait for EOF on the registration socket");
    return Reflect.apply(read, receiver, args);
  },
});
