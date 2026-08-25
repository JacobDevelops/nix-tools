import kleur from "kleur";

export const greeting = (name: string): string => kleur.green(`hello ${name}`);
