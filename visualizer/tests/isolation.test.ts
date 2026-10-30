import assert from 'node:assert/strict';
import test from 'node:test';
import { bandHz, isolationKey, paintRange, pickPolyline, remapLit, segmentDistanceSq } from '../src/isolation';

test('paintRange lights an inclusive range in either direction and clamps', () => {
  const lit = new Array<boolean>(8).fill(false);
  assert.deepEqual(paintRange(lit, 5, 2, true).map(Number).join(''), '00111100');
  assert.deepEqual(paintRange(paintRange(lit, 0, 7, true), -3, 1, false).map(Number).join(''), '00111111');
  assert.deepEqual(lit.some(Boolean), false, 'input is not mutated');
});

test('remapLit keeps lit frequencies across a band count change', () => {
  const lit = paintRange(new Array<boolean>(8).fill(false), 2, 3, true);
  assert.equal(remapLit(lit, 16).map(Number).join(''), '0000111100000000');
  assert.equal(remapLit(remapLit(lit, 16), 8).map(Number).join(''), '00110000');
  assert.deepEqual(remapLit(lit, 8), lit);
  assert.equal(remapLit([], 4).length, 4);
});

test('band edges are log spaced from 20 Hz to Nyquist like the daemon', () => {
  const [start] = bandHz(0, 64, 48_000);
  const [, end] = bandHz(63, 64, 48_000);
  assert.equal(start, 20);
  assert.ok(Math.abs(end - 24_000) < 1e-6);
  const [a0, a1] = bandHz(10, 64, 48_000), [b0, b1] = bandHz(50, 64, 48_000);
  assert.ok(Math.abs(a1 / a0 - b1 / b0) < 1e-6, 'every band spans the same ratio');
});

test('isolationKey is empty when nothing is lit', () => {
  assert.equal(isolationKey(8, new Array<boolean>(8).fill(false)), '');
  assert.equal(isolationKey(8, paintRange(new Array<boolean>(8).fill(false), 1, 2, true)), '8:1,2');
});

test('pickPolyline returns the nearest visible line within tolerance', () => {
  assert.equal(segmentDistanceSq(5, 5, 0, 0, 10, 0), 25);
  // Three horizontal lines at y = 10, 30, 50 spanning x 0..100; the second is hidden behind the camera.
  const screen = (band: number, column: number, out: [number, number]) => {
    out[0] = column; out[1] = 10 + band * 20;
    return band !== 1;
  };
  assert.equal(pickPolyline(3, 101, 40, 14, 8, screen), 0);
  assert.equal(pickPolyline(3, 101, 40, 45, 8, screen), 2);
  assert.equal(pickPolyline(3, 101, 40, 31, 8, screen), null, 'a hidden line is never picked');
  assert.equal(pickPolyline(3, 101, 40, 80, 8, screen), null);
  assert.equal(pickPolyline(3, 101, 200, 50, 8, screen), null, 'beyond the end of every line');
});
