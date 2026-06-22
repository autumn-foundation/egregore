// Test file for the TypeScript scan fixture.
import { Widget, WidgetKind, helper, answer } from "./widget";

function testWidgetRuns() {
  const w = new Widget(1);
  if (w.run() !== 1) throw new Error("expected 1");
}

function testAnswer() {
  if (answer() !== 42) throw new Error("expected 42");
}

function testHelper() {
  if (helper(5) !== 5) throw new Error("expected 5");
}
