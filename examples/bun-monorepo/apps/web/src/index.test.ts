import { expect, test } from "bun:test";
import { view } from "./index";

test("uses the shared greeting", () => {
  expect(view("web").props.children).toContain("hello web");
});
