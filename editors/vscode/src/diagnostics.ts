/** Regex to parse Phoenix diagnostic output: file:line:col: severity: message */
const DIAGNOSTIC_RE = /^(.+?):(\d+):(\d+): (error|warning): (.+)$/;

/** A parsed diagnostic from `phoenix check` stderr output. */
export interface ParsedDiagnostic {
  file: string;
  /** 1-based line number from the CLI output. */
  line: number;
  /** 1-based column number from the CLI output. */
  col: number;
  severity: "error" | "warning";
  message: string;
}

/**
 * Parses Phoenix CLI stderr output into structured diagnostics.
 *
 * Each line matching `file:line:col: error|warning: message` produces a
 * {@link ParsedDiagnostic}. Non-matching lines (blank lines, hints) are
 * silently skipped.
 */
export function parseDiagnosticOutput(stderr: string): ParsedDiagnostic[] {
  const diagnostics: ParsedDiagnostic[] = [];

  for (const line of stderr.split("\n")) {
    const match = DIAGNOSTIC_RE.exec(line.trim());
    if (!match) continue;

    const [, file, lineStr, colStr, severity, message] = match;
    diagnostics.push({
      file,
      line: parseInt(lineStr, 10),
      col: parseInt(colStr, 10),
      severity: severity as "error" | "warning",
      message,
    });
  }

  return diagnostics;
}
