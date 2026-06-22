// Widget module for the TypeScript scan fixture.
// This file has extra leading comments to test byte-shift stability.
// The logic below is identical to the non-shifted fixture.
import { EventEmitter } from "events";
import * as path from "path";
import type { Readable } from "stream";

export const LIMIT = 7;
export const NAME: string = "basic";

export interface Describable {
  describe(): string;
}

export type WidgetId = string;

export enum WidgetKind {
  Simple = "simple",
  Advanced = "advanced",
}

export class Base implements Describable {
  describe(): string {
    return "base";
  }
}

export class Widget extends Base {
  private id: WidgetId;
  kind: WidgetKind;

  constructor(value: number, kind: WidgetKind = WidgetKind.Simple) {
    super();
    this.id = String(value);
    this.kind = kind;
  }

  run(): number {
    return helper(Number(this.id));
  }

  describe(): string {
    return `widget-${this.id}`;
  }
}

export function helper(value: number): number {
  return value;
}

export function answer(): number {
  return new Widget(42).run();
}

export const arrowHelper = (x: number): number => x + 1;
