import { describe, it, expect } from "vitest";
import { parseDiagnosticOutput } from "../diagnostics";

describe("parseDiagnosticOutput", () => {
  it("parses a single error", () => {
    const stderr = 'test.phx:3:5: error: unknown type "Usr"\n';
    const result = parseDiagnosticOutput(stderr);
    expect(result).toEqual([
      {
        file: "test.phx",
        line: 3,
        col: 5,
        severity: "error",
        message: 'unknown type "Usr"',
      },
    ]);
  });

  it("parses a warning", () => {
    const stderr = "schema.phx:10:1: warning: unused variable\n";
    const result = parseDiagnosticOutput(stderr);
    expect(result).toEqual([
      {
        file: "schema.phx",
        line: 10,
        col: 1,
        severity: "warning",
        message: "unused variable",
      },
    ]);
  });

  it("parses multiple diagnostics", () => {
    const stderr = [
      'api.phx:1:15: error: unknown type "Usr"',
      "api.phx:5:3: error: body is not allowed on Get endpoints",
      "",
    ].join("\n");
    const result = parseDiagnosticOutput(stderr);
    expect(result).toHaveLength(2);
    expect(result[0].line).toBe(1);
    expect(result[0].message).toBe('unknown type "Usr"');
    expect(result[1].line).toBe(5);
    expect(result[1].message).toBe("body is not allowed on Get endpoints");
  });

  it("returns empty array for empty stderr", () => {
    expect(parseDiagnosticOutput("")).toEqual([]);
  });

  it("skips blank lines and non-matching lines", () => {
    const stderr = [
      "",
      "  hint: did you mean User?",
      "some random output",
      'test.phx:1:1: error: syntax error: expected "{"',
      "",
    ].join("\n");
    const result = parseDiagnosticOutput(stderr);
    expect(result).toHaveLength(1);
    expect(result[0].message).toBe('syntax error: expected "{"');
  });

  it("handles file paths with directories", () => {
    const stderr =
      '/home/user/project/api/schema.phx:42:10: error: field "id" does not exist\n';
    const result = parseDiagnosticOutput(stderr);
    expect(result).toHaveLength(1);
    expect(result[0].file).toBe("/home/user/project/api/schema.phx");
    expect(result[0].line).toBe(42);
    expect(result[0].col).toBe(10);
  });

  it("handles Windows-style paths", () => {
    const stderr =
      'C:\\Users\\dev\\schema.phx:7:3: error: unknown struct "Widget"\n';
    const result = parseDiagnosticOutput(stderr);
    expect(result).toHaveLength(1);
    expect(result[0].file).toBe("C:\\Users\\dev\\schema.phx");
  });

  it("handles messages containing colons", () => {
    const stderr =
      'test.phx:1:1: error: endpoint `createUser`: `body` is not allowed on Get endpoints\n';
    const result = parseDiagnosticOutput(stderr);
    expect(result).toHaveLength(1);
    expect(result[0].message).toBe(
      "endpoint `createUser`: `body` is not allowed on Get endpoints"
    );
  });

  it("handles line 1 col 1 (minimum position)", () => {
    const stderr = "test.phx:1:1: error: unexpected token\n";
    const result = parseDiagnosticOutput(stderr);
    expect(result[0].line).toBe(1);
    expect(result[0].col).toBe(1);
  });

  it("handles large line and column numbers", () => {
    const stderr = "big.phx:9999:256: error: some error\n";
    const result = parseDiagnosticOutput(stderr);
    expect(result[0].line).toBe(9999);
    expect(result[0].col).toBe(256);
  });
});
