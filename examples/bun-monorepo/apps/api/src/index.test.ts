import { expect, test } from "bun:test";
import { app } from "./index";

test("serves the shared greeting", async () => {
  expect(await (await app.request("/")).text()).toContain("hello api");
});
