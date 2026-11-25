import { expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { PassThrough } from "node:stream";
import { buildFlags, buildFrame, encodeFrame, FrameType, Priority, SubcSocket } from "../src/index.js";
import type { ConfigFileError, ConfigPendingSettingsStatus, ConfigRevertReply, ConfigRevertRequest, ConfigSettingsFileChange, ConfigStatusFileError, ConfigWatchFileErrorRow } from "../src/index.js";

const folder = join(import.meta.dir, "../../../vectors/config-settings-v1");
const wireVectors = readdirSync(folder).filter((name) => /^(project-|revert-|settings-card-open).*\.json$/.test(name)).sort();
if (!wireVectors.length) throw new Error("missing settings vectors");

for (const name of wireVectors) {
  test(`channel-0 JSON decodes ${name}`, async () => {
    const sample = JSON.parse(readFileSync(join(folder, name), "utf8"));
    const frames: [FrameType, object][] = [];
    if (sample.request) {
      frames.push([FrameType.Request, sample.request]);
      if (sample.reply) frames.push([sample.reply.code ? FrameType.Error : FrameType.Response, sample.reply]);
    } else {
      frames.push([sample.op === "config.watch" ? FrameType.StreamData : FrameType.Push, sample]);
    }

    const stream = new PassThrough();
    const socket = SubcSocket.adopt(stream);
    const flags = buildFlags(false, Priority.Passive, false);
    try {
      for (const [ty, body] of frames) {
        const corr = ty === FrameType.Push ? 0n : 1n;
        stream.write(encodeFrame(buildFrame(ty, flags, 0, 0, corr, Buffer.from(JSON.stringify(body), "utf8"))));
        const deadline = Date.now() + 1000;
        const frame = await socket.readFrame(deadline, deadline);
        expect(frame.header).toMatchObject({ ty, flags, channel: 0, epoch: 0, corr });
        const decoded = JSON.parse(Buffer.from(frame.body).toString("utf8"));
        expect(decoded).toEqual(body);
        if (name === "project-watch.change.json") {
          const row: ConfigWatchFileErrorRow = decoded;
          const error: ConfigFileError | undefined = row.file_error;
          expect(error?.layer).toBe("project");
          expect(error?.path).toBe("/home/example/repos/private/.cortexkit/a.jsonc");
        }
        if (name === "project-status-workspace-file-error.json" && ty === FrameType.Response) {
          const errors: ConfigStatusFileError[] = decoded.status.file_errors;
          expect(errors.map(({ project_id }) => project_id)).toEqual(["A", "B"]);
          for (const { layer, path } of errors) {
            expect(layer).toBe("workspace");
            expect(path).toBe("/home/example/repos/workspace/.cortexkit/a.jsonc");
          }
        }
        if (name === "settings-card-open.json" && ty === FrameType.Response) {
          const status: ConfigPendingSettingsStatus = decoded.status;
          expect(status.pending_settings_card).toBe(sample.plan_get_view.digest);
          const summary: ConfigSettingsFileChange = sample.plan_get_view.settings_file_change;
          expect(summary.files.map(({ layer }) => layer)).toEqual(["user"]);
          expect(sample.plan_get_view.plan.settings_file_change).toBeUndefined();
        }
        if (name === "revert-success.json") {
          const message: ConfigRevertRequest | ConfigRevertReply = decoded;
          expect(message.op).toBe("config.revert");
          expect("digest" in message).toBe(ty === FrameType.Request);
          if ("files" in message) {
            expect(message.files).toEqual([{ path: "/home/example/cortexkit/demo.jsonc", status: "restored" }]);
          }
        }
        if (name === "revert-changed.json" && ty === FrameType.Error) {
          expect(decoded.detail.files).toEqual([{
            path: "/home/example/cortexkit/demo.jsonc", status: "refused",
            code: decoded.code, message: decoded.message,
          }]);
        }
      }
    } finally { socket.close(); }
  });
}

test("a workspace card groups one file with per-project effects", () => {
  const { view_projection: view } = JSON.parse(readFileSync(join(folder, "workspace-settings-card.json"), "utf8"));
  const { files }: ConfigSettingsFileChange = view.settings_file_change;
  expect(files).toHaveLength(1);
  expect(files[0]?.layer).toBe("workspace");
  expect(files[0]?.affected_projects).toEqual(view.plan.changeset.fields.map(({ project_id }: { project_id: string }) => project_id));
});
