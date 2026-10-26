"""Human-like pacing for AI browsing.

Two parts:
- Timing (this module's original job): :func:`pace`, :func:`jitter`,
  :class:`Pacer` — irregular delays between actions, never fixed
  intervals, never bursts.
- Pointer paths (:func:`human_path`): a human-like mouse trajectory
  between two pixel coordinates — variable speed, wobbling path,
  random pauses, occasional overshoot-and-pullback. Never a
  constant-speed straight line (that is a robot signature, and the
  user's slider experience confirms it: too perfect always fails).

Pointer paths are consumed by :mod:`yousj.mouse` (the simulated mouse).
This module only generates the path; it moves no cursor (there is no
real screen — see :mod:`yousj.mouse`).

Typical use::

    from yousj import human
    human.pace()              # sleep a random 0.5–1.5 s before acting
    path = human.human_path(0, 0, 400, 200)   # [(x, y, delay), ...]
    for x, y, delay in path:
        time.sleep(delay)     # or let yousj.mouse.Mouse do it
        move_pointer_to(x, y)
"""
import math
import random
import time

DEFAULT_MIN_S = 0.5
DEFAULT_MAX_S = 1.5


def pace(min_s: float = DEFAULT_MIN_S, max_s: float = DEFAULT_MAX_S) -> float:
    """Sleep a random duration in [min_s, max_s] seconds.

    Uniform-random, never a fixed interval. Returns the slept seconds.
    ``min_s`` must be >= 0 and ``max_s`` >= ``min_s``.
    """
    if min_s < 0:
        raise ValueError("min_s must be >= 0")
    if max_s < min_s:
        raise ValueError("max_s must be >= min_s")
    slept = random.uniform(min_s, max_s)
    time.sleep(slept)
    return slept


def jitter(base: float, factor: float = 0.3) -> float:
    """Scale ``base`` by a random factor in [1-factor, 1+factor].

    Use it to humanize any fixed delay: ``time.sleep(human.jitter(1.0))``
    sleeps ~0.7–1.3 s instead of exactly 1.0 s.
    """
    if base < 0:
        raise ValueError("base must be >= 0")
    if not 0 <= factor < 1:
        raise ValueError("factor must be in [0, 1)")
    return base * random.uniform(1.0 - factor, 1.0 + factor)


class Pacer:
    """Enforces human spacing across consecutive operations.

    ``pace()`` sleeps until at least ``min_gap`` seconds have passed
    since the previous ``pace()`` call, plus a random extra up to
    ``max_extra``. This prevents bursts even when the caller forgets to
    wait: two back-to-back ``pace()`` calls never fire instantly.
    """

    def __init__(self, min_gap: float = 0.5, max_extra: float = 1.0):
        if min_gap < 0 or max_extra < 0:
            raise ValueError("gaps must be >= 0")
        self.min_gap = min_gap
        self.max_extra = max_extra
        self._last = None

    def pace(self) -> float:
        """Wait for a human-like gap, then mark this action done.

        If the caller already spent longer than ``min_gap`` doing work,
        only a random extra (0..``max_extra``) is slept. If the caller
        is bursting, the remaining gap plus a random extra is slept.
        Never a fixed interval. Returns the slept seconds.
        """
        now = time.monotonic()
        if self._last is None:
            wait = random.uniform(0, self.max_extra)
        else:
            elapsed = now - self._last
            wait = max(self.min_gap - elapsed, 0.0) + random.uniform(
                0, self.max_extra)
        time.sleep(wait)
        self._last = time.monotonic()
        return wait

    def reset(self):
        """Forget the last action time (next pace() starts fresh)."""
        self._last = None


def human_path(x0, y0, x1, y1, rng=None, overshoot=None):
    """Human-like pointer path from (x0, y0) to (x1, y1).

    Returns a list of ``(x, y, delay)`` points: the pointer visits each
    point in order, sleeping ``delay`` seconds BEFORE moving to that
    point (the first point's delay is 0 — we're already there). The
    last point is exactly ``(x1, y1)``.

    Human properties (never a constant-speed straight line):
    - Speed profile: slow start, faster middle, slow approach
      (smoothstep easing) with per-point random variation (0.6x-1.6x),
      so no two segments take the same time.
    - Wobble: perpendicular sine wave plus random noise (amplitude
      scales with distance, ~2-8 px), so the path is visibly curved,
      not a ruler line.
    - Pauses: 0-2 random mid-path pauses (0.05-0.35 s extra) — humans
      hesitate mid-drag.
    - Overshoot: with probability ~0.35 (or forced via
      ``overshoot=True`` / forbidden via ``overshoot=False``), the path
      passes the target by 5-25 px along the direction, hesitates, then
      pulls back in 2-4 points to exactly ``(x1, y1)`` — the user's
      slider lesson (drag past, then pull back) built in.
    - Point spacing: ~8-16 px apart (randomized), so long drags have
      many points and short hops have few.

    ``rng`` is a ``random.Random`` instance for deterministic tests;
    the default is the global ``random`` module. Coordinates are ints
    (pixels). A zero-distance move returns ``[(x1, y1, 0.0)]``.
    """
    r = rng or random
    x0, y0, x1, y1 = int(x0), int(y0), int(x1), int(y1)
    dx, dy = x1 - x0, y1 - y0
    dist = math.hypot(dx, dy)
    if dist < 1.0:
        return [(x1, y1, 0.0)]
    ux, uy = dx / dist, dy / dist          # direction unit vector
    px, py = -uy, ux                        # perpendicular unit vector
    step = r.uniform(8.0, 16.0)
    n = max(2, min(200, int(dist / step)))
    # Total time scales with distance (a 400 px drag takes ~0.6-1.5 s).
    total = (0.3 + dist * 0.0012) * r.uniform(0.7, 1.3)
    base_delay = total / n
    freq = r.uniform(1.5, 3.5)              # wobble half-waves over path
    amp = min(8.0, dist * 0.02 + 2.0) * r.uniform(0.5, 1.0)
    phase = r.uniform(0, math.pi * 2)
    points = []
    for i in range(n + 1):
        t = i / n
        ease = t * t * (3.0 - 2.0 * t)      # smoothstep: slow-fast-slow
        bx = x0 + ease * dx
        by = y0 + ease * dy
        wob = math.sin(t * math.pi * freq + phase) * amp * math.sin(
            math.pi * t)  # fade wobble to 0 at both ends
        nx = r.uniform(-1.5, 1.5)
        ny = r.uniform(-1.5, 1.5)
        x = int(round(bx + px * wob + nx))
        y = int(round(by + py * wob + ny))
        delay = 0.0 if i == 0 else base_delay * r.uniform(0.6, 1.6)
        points.append((x, y, delay))
    # Random mid-path pauses (not on the first/last point).
    if n >= 4:
        for _ in range(r.randint(0, 2)):
            j = r.randint(1, n - 1)
            x, y, d = points[j]
            points[j] = (x, y, d + r.uniform(0.05, 0.35))
    # Force the last point exactly onto the target (wobble/noise free).
    points[-1] = (x1, y1, points[-1][2])
    # Overshoot-and-pullback: pass the target, hesitate, come back.
    do_overshoot = overshoot if overshoot is not None else r.random() < 0.35
    if do_overshoot:
        over = r.uniform(5.0, min(25.0, dist * 0.15 + 5.0))
        ox = int(round(x1 + ux * over + px * r.uniform(-2, 2)))
        oy = int(round(y1 + uy * over + py * r.uniform(-2, 2)))
        points.append((ox, oy, base_delay + r.uniform(0.05, 0.2)))
        back = r.randint(2, 4)
        for k in range(1, back):
            t = k / back
            points.append((
                int(round(ox + (x1 - ox) * t)),
                int(round(oy + (y1 - oy) * t)),
                base_delay * r.uniform(0.6, 1.4)))
        points.append((x1, y1, base_delay * r.uniform(0.6, 1.2)))
    return points
