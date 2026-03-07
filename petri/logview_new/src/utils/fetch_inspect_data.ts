// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { InspectNode, InspectObject } from "../data_defs";

function parseInspectNode(input: string): InspectObject {
  let i = 0;

  function skipWhitespace() {
    while (/\s/.test(input[i])) i++;
  }

  function parseKey(): string {
    skipWhitespace();
    const match = /^(.+?):\s/.exec(input.slice(i));
    if (!match)
      throw new Error(
        `Invalid key at position ${i}: '${input.slice(i, i + 10)}'`
      );
    i += match[0].length;
    return match[1];
  }

  function parseString(): string {
    i++;
    let str = "";
    while (i < input.length && input[i] !== '"') {
      if (input[i] === "\\") str += input[i++];
      str += input[i++];
    }
    if (input[i] !== '"') throw new Error("Unterminated string");
    i++;
    return str;
  }

  function parseValue(): InspectNode {
    skipWhitespace();
    if (input[i] === "{") return parseObject();
    if (input[i] === '"') return { type: "string", value: parseString() };
    if (input[i] === "<") {
      const start = i;
      while (i < input.length && input[i] !== ">") i++;
      i++;
      return { type: "bytes", value: input.slice(start, i) };
    }
    if (input[i] === "_") {
      i++;
      return { type: "unevaluated" };
    }
    if (input[i] === "t") {
      if (input.slice(i, i + 4) !== "true")
        throw new Error(`Expected 'true' at ${i}`);
      i += 4;
      return { type: "boolean", value: true };
    }
    if (input[i] === "f") {
      if (input.slice(i, i + 5) !== "false")
        throw new Error(`Expected 'false' at ${i}`);
      i += 5;
      return { type: "boolean", value: false };
    }
    if (input[i] === "e") {
      if (input.slice(i, i + 7) !== "error (")
        throw new Error(`Expected 'error (' at ${i}`);
      i += 7;
      let parens = 1;
      const start = i;
      while (i < input.length && parens > 0) {
        if (input[i] === "(") parens++;
        else if (input[i] === ")") parens--;
        i++;
      }
      if (input[i - 1] !== ")") throw new Error("Unterminated error");
      return { type: "error", value: input.slice(start, i - 1) };
    }
    const match = /^[+-]?((0x[0-9a-fA-F]+)|(0b[01]+)|([0-9]+(\.[0-9]*)?))/.exec(
      input.slice(i)
    );
    if (match) {
      i += match[0].length;
      return { type: "number", value: match[0] };
    }
    throw new Error(`Unexpected token at ${i}: '${input.slice(i, i + 10)}'`);
  }

  function parseObject(): InspectObject {
    if (input[i] !== "{") throw new Error(`Expected '{' at ${i}`);
    i++;
    skipWhitespace();
    const children: { key: string; value: InspectNode }[] = [];
    while (i < input.length && input[i] !== "}") {
      const key = parseKey();
      skipWhitespace();
      const value = parseValue();
      children.push({ key, value });
      skipWhitespace();
      if (input[i] === ",") {
        i++;
        skipWhitespace();
      } else if (input[i] !== "}")
        throw new Error(`Expected ',' or '}' at ${i}`);
    }
    if (input[i] !== "}") throw new Error(`Unterminated object at ${i}`);
    i++;
    return { type: "object", children };
  }

  skipWhitespace();
  const result = parseObject();
  skipWhitespace();
  if (i < input.length) throw new Error(`Trailing content at ${i}`);
  return result;
}

/**
 * Fetches and parses an inspect file from the given URL.
 * Returns the parsed inspect object structure containing the file's data tree.
 */
export async function getInspectFile(fileUrl: string): Promise<InspectObject> {
  const resp = await fetch(fileUrl);
  if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
  const text = await resp.text();
  return parseInspectNode(text);
}
