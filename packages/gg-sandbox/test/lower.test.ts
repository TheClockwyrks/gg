// The read lowering gg.files.readFile and gg.views.openFile share
// (src/internal/lower.ts).
import { describe, expect, it } from "vitest";

import { asFileRead } from "../src/internal/lower.js";

describe("asFileRead", () => {
  it("flattens a text read into the text arm", () => {
    const val = {
      contents: "a\nb",
      firstLine: 1,
      lastLine: 2,
      totalLines: 2,
      byteTruncated: false,
    };
    expect(asFileRead({ tag: "text", val })).toEqual({ kind: "text", ...val });
  });

  it("flattens an image read, with its byte count a number rather than a bigint", () => {
    const read = asFileRead({
      tag: "image",
      val: {
        mediaType: "image/png",
        label: "PNG",
        bytes: 2048n,
        shown: false,
        notShownReason: "too large",
      },
    });
    expect(read).toEqual({
      kind: "image",
      mediaType: "image/png",
      label: "PNG",
      bytes: 2048,
      shown: false,
      notShownReason: "too large",
    });
    // A bigint anywhere in a returned structure makes JSON.stringify throw.
    expect(() => JSON.stringify(read)).not.toThrow();
  });
});
