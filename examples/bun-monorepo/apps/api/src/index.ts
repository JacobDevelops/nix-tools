import { greeting } from "@example/shared";
import { Hono } from "hono";

export const app = new Hono().get("/", (context) => context.text(greeting("api")));

if (import.meta.main) console.log(greeting("api"));
