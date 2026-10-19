// The failure normaliser and the argument validators every capability module
// runs (src/internal/errors.ts), run under Node rather than inside the guest:
// they touch no membrane import, so what they make of a value is the same here.
import { describe, expect, it } from "vitest";

import { ApiError } from "../src/gg/core.js";
import {
  arrayArg,
  asApiError,
  call,
  describeThrown,
  errorMessage,
  errorName,
  isErrorLike,
  opts,
  positive,
  requireString,
  typeName,
  U32_MAX,
  uint,
} from "../src/internal/errors.js";

/** The ApiError a call threw, or a failure naming what it threw instead. */
function thrownBy(fn: () => unknown): ApiError {
  try {
    fn();
  } catch (thrown) {
    expect(thrown).toBeInstanceOf(ApiError);
    return thrown as ApiError;
  }
  throw new Error("the call threw nothing");
}

describe("asApiError", () => {
  it("lowers a membrane record carried under `payload` into an ApiError", () => {
    const thrown = {
      payload: {
        operation: "read_file",
        code: "not-found",
        message: "no such file",
      },
    };
    const error = asApiError(thrown);
    expect(error).toBeInstanceOf(ApiError);
    expect(error).toMatchObject({
      name: "ApiError",
      operation: "read_file",
      code: "not-found",
      message: "no such file",
    });
  });

  it("lowers a bare membrane record too", () => {
    const error = asApiError({
      operation: "shell",
      code: "timeout",
      message: "took too long",
    });
    expect(error).toBeInstanceOf(ApiError);
    expect((error as ApiError).code).toBe("timeout");
  });

  it("returns an ApiError untouched, keeping its stack", () => {
    const original = new ApiError("finish", "invalid-argument", "bad");
    expect(asApiError(original)).toBe(original);
  });

  it("leaves a value that is not a membrane failure alone", () => {
    const typeError = new TypeError("x is undefined");
    expect(asApiError(typeError)).toBe(typeError);
    expect(asApiError("a string")).toBe("a string");
    expect(asApiError(null)).toBeNull();
    expect(asApiError({ operation: "shell", code: 3 })).toEqual({
      operation: "shell",
      code: 3,
    });
  });
});

describe("call", () => {
  it("returns what the call returned", () => {
    expect(call(() => 42)).toBe(42);
  });

  it("throws a thrown membrane record as an ApiError", () => {
    const error = thrownBy(() =>
      call(() => {
        throw {
          payload: { operation: "write_file", code: "denied", message: "no" },
        };
      }),
    );
    expect(error.operation).toBe("write_file");
  });
});

describe("the error-like helpers", () => {
  it("recognise an Error from another realm by its brand", () => {
    const foreign = Object.create(null) as Record<symbol, string>;
    foreign[Symbol.toStringTag] = "Error";
    expect(isErrorLike(new RangeError("r"))).toBe(true);
    expect(isErrorLike(foreign)).toBe(true);
    expect(isErrorLike({ message: "looks like one" })).toBe(false);
    expect(isErrorLike("text")).toBe(false);
  });

  it("read a name and a message whatever the error carries", () => {
    const nameless = new Error("m");
    nameless.name = "";
    expect(errorName(nameless)).toBe("Error");
    expect(errorName(new TypeError("t"))).toBe("TypeError");
    const odd = { message: 7 } as unknown as Error;
    expect(errorMessage(odd)).toBe(String(odd));
    expect(errorMessage(new Error("plain"))).toBe("plain");
  });

  it("describe a thrown value that serialises to nothing", () => {
    expect(describeThrown("text")).toBe("text");
    expect(describeThrown({ a: 1 })).toBe('{"a":1}');
    expect(describeThrown(42)).toBe("42");
    expect(describeThrown(new Map())).toBe(
      "the program threw a value that is not an Error: [object Map]",
    );
    const cyclic: { self?: unknown } = {};
    cyclic.self = cyclic;
    expect(describeThrown(cyclic)).toBe(
      "the program threw a value that is not an Error: [object Object], own properties: self",
    );
  });
});

describe("the argument validators", () => {
  it("name what a value is, telling an array and null from an object", () => {
    expect(typeName(null)).toBe("null");
    expect(typeName([])).toBe("array");
    expect(typeName({})).toBe("object");
    expect(typeName(3)).toBe("number");
  });

  it("take an options object and refuse a positional argument", () => {
    expect(opts("shell", undefined)).toBeUndefined();
    expect(opts("shell", null)).toBeUndefined();
    expect(opts("shell", { timeoutSecs: 5 })).toEqual({ timeoutSecs: 5 });
    const error = thrownBy(() => opts("shell", 300));
    expect(error).toMatchObject({
      operation: "shell",
      code: "invalid-argument",
      message: "expected an options object, got number",
    });
    expect(thrownBy(() => opts("shell", [1])).message).toBe(
      "expected an options object, got array",
    );
  });

  it("take a whole number in range and refuse one the membrane would wrap", () => {
    expect(uint("read_file", "offset", undefined, U32_MAX)).toBeUndefined();
    expect(uint("read_file", "offset", 0, U32_MAX)).toBe(0);
    expect(uint("read_file", "offset", U32_MAX, U32_MAX)).toBe(U32_MAX);
    for (const value of [-1, 1.5, U32_MAX + 1, "3"]) {
      expect(
        thrownBy(() => uint("read_file", "offset", value, U32_MAX)).message,
      ).toBe(
        `\`offset\` must be a whole number 0..${U32_MAX}, got ${String(value)}`,
      );
    }
  });

  it("take a finite positive number", () => {
    expect(positive("shell", "timeoutSecs", 0.5)).toBe(0.5);
    expect(positive("shell", "timeoutSecs", null)).toBeUndefined();
    for (const value of [0, -2, Number.POSITIVE_INFINITY, Number.NaN]) {
      expect(
        thrownBy(() => positive("shell", "timeoutSecs", value)).message,
      ).toBe(`\`timeoutSecs\` must be a positive number, got ${String(value)}`);
    }
  });

  it("default a missing list to empty and refuse anything but an array", () => {
    expect(arrayArg("add_task", "blockedBy", undefined)).toEqual([]);
    expect(arrayArg("add_task", "blockedBy", ["a"])).toEqual(["a"]);
    expect(thrownBy(() => arrayArg("add_task", "blockedBy", "a")).message).toBe(
      "`blockedBy` must be an array, got string",
    );
  });

  it("require a string", () => {
    expect(() => requireString("finish", "a summary", "done")).not.toThrow();
    expect(
      thrownBy(() => requireString("finish", "a summary", 42)),
    ).toMatchObject({
      operation: "finish",
      message: "expected a summary, got number",
    });
  });
});

describe("ApiError", () => {
  it("serialises with its message", () => {
    const error = new ApiError("shell", "timeout", "took too long");
    expect(JSON.parse(JSON.stringify(error))).toEqual({
      name: "ApiError",
      operation: "shell",
      code: "timeout",
      message: "took too long",
    });
  });
});
