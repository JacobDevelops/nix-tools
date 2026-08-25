import { greeting } from "@example/shared";
import { h } from "preact";

export const view = (name: string) => h("main", null, greeting(name));

if (import.meta.main) console.log(view("web"));
