"""Signals: one object per observable quantity, with the verbs on it.

The unit of the v2 surface is not the component or the site — it is the
*signal*: a battery's SoC, a meter's power, the site's grid power. Each
is an object with up to three verbs, and the capability set lives in the
type:

- :class:`Signal` — ``read`` / ``try_read`` / ``expect``. Instantaneous:
  ``expect`` settles, and ``hold_for=`` asserts across a window.
- :class:`CumulativeSignal` — same reads; ``expect`` checks the running
  total once (its signature has no ``hold_for``).
- :class:`DrivenSignal` — a readable signal the *simulator* also lets
  you set (a meter's published power, a battery's SoC). ``set`` is the
  test-side stimulus; it is not the production command path.
- :class:`SettingSignal` — write-only knobs (health, sunlight).

Component signals hang off the topology builders once launched
(``LOAD.power``, ``BAT.soc``); aggregates hang off the site
(``site.grid_power``). Same shapes everywhere.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Awaitable, Callable
from datetime import timedelta
from typing import Any, Generic, TypeVar

from frequenz.quantities import Quantity

from .assertions import _read_value, expect_metric
from .errors import NoSample
from .matchers import Matcher
from .metrics import MetricSpec

Q = TypeVar("Q", bound=Quantity)
V = TypeVar("V")

CheckRef = Callable[[], tuple[int, str]]
"""Resolves a signal's scenario identity: (component id, metric symbol)."""

CueForm = Callable[[Any], str]
"""Renders a timed scenario cue setting the signal to a value."""

Kind = type | tuple[type, ...]
"""The type (or types) a value must have."""

_READ_WAIT = timedelta(seconds=5)
_TIMEOUT = timedelta(seconds=10)
_POLL = timedelta(milliseconds=250)


def _require(value: object, kind: Kind, name: str) -> None:
    """Raise :class:`TypeError` unless ``value`` is a ``kind``.

    A bare number carries no unit, so a quantity argument refuses one.
    """
    if not isinstance(value, kind):
        kinds = kind if isinstance(kind, tuple) else (kind,)
        wanted = " or ".join(k.__name__ for k in kinds)
        raise TypeError(f"{name} must be {wanted}, got {type(value).__name__}")


class _Readable(Generic[Q]):
    """The read surface every signal shares."""

    def __init__(
        self,
        spec: MetricSpec[Q],
        read: Callable[[], Awaitable[Q | None]],
        label: str,
        *,
        check_ref: CheckRef | None = None,
    ) -> None:
        self._spec = spec
        self._read = read
        self._label = label
        self._check_ref = check_ref

    def _scenario_check_ref(self) -> tuple[int, str]:
        """The (component id, metric symbol) a scenario check targets."""
        if self._check_ref is None:
            raise ValueError(
                f"{self._label}: scenario checks target component signals "
                "(power, soc); site aggregates are not checkable yet"
            )
        return self._check_ref()

    async def try_read(self) -> Q | None:
        """The latest value, or ``None`` while none is available."""
        return await _read_value(self._read)

    async def read(self, *, wait: timedelta = _READ_WAIT) -> Q:
        """The latest value; awaits availability up to ``wait``.

        Raises:
            NoSample: no value became available within ``wait``.

        Returns:
            The value — never ``None``, so it composes into plain asserts.
        """
        deadline = time.monotonic() + wait.total_seconds()
        value = await _read_value(self._read)
        while value is None and time.monotonic() < deadline:
            await asyncio.sleep(0.05)
            value = await _read_value(self._read)
        if value is None:
            raise NoSample(f"{self._label}: no sample within {wait}")
        return value


class Signal(_Readable[Q]):
    """An instantaneous, assertable quantity on a running site."""

    async def expect(
        self,
        matcher: Matcher[Q],
        *,
        hold_for: timedelta | None = None,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Q | list[Q | None] | None:
        """Settle until ``matcher`` holds — or, with ``hold_for``, require
        it on every sample across that window.

        ``timeout`` is the settle budget; with ``hold_for`` the window
        itself is the budget and ``timeout`` does not apply.
        """
        return await expect_metric(
            self._spec.bind(self._read, label=self._label),
            approx=matcher.approx,
            tol=matcher.tol,
            within=matcher.within,
            max=matcher.max,
            min=matcher.min,
            for_=hold_for,
            timeout=timeout,
            poll=poll,
        )


class CumulativeSignal(_Readable[Q]):
    """A monotonic running total (energy): checked once, when available.

    Deliberately NOT a :class:`Signal` subclass: its ``expect`` has no
    ``hold_for``, because a running total is not a settling signal — the
    kind distinction is the method's signature, not a runtime error.
    """

    async def expect(
        self,
        matcher: Matcher[Q],
        *,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Q | list[Q | None] | None:
        """Check ``matcher`` once, after awaiting the total's first value."""
        return await expect_metric(
            self._spec.bind(self._read, label=self._label),
            approx=matcher.approx,
            tol=matcher.tol,
            within=matcher.within,
            max=matcher.max,
            min=matcher.min,
            timeout=timeout,
            poll=poll,
        )


class _Settable(Generic[V]):
    """The write surface every settable signal shares.

    ``kind`` is the type ``set`` takes and ``cue_kind`` (default ``kind``)
    the type a scenario cue takes; any other value raises
    :class:`TypeError`.
    """

    def __init__(
        self,
        set_: Callable[[V], Awaitable[None]],
        label: str,
        *,
        kind: Kind,
        cue_kind: Kind | None = None,
        cue: CueForm | None = None,
    ) -> None:
        self._set = set_
        self._label = label
        self._kind = kind
        self._cue_kind = kind if cue_kind is None else cue_kind
        self._cue = cue

    async def set(self, value: V) -> None:
        """Set the signal to ``value``; a rejection raises
        ``ControlRejected``."""
        _require(value, self._kind, self._label)
        await self._set(value)

    def _scenario_cue(self, value: V) -> str:
        """Render the Lisp form a scenario cue uses to set this signal."""
        if self._cue is None:
            raise ValueError(f"{self._label}: this signal cannot be cued")
        _require(value, self._cue_kind, self._label)
        return self._cue(value)


class DrivenSignal(Signal[Q], _Settable[Q]):
    """A signal the simulator lets the test set directly.

    ``set`` arranges the world (a meter's published load, a battery's
    SoC). It is a test-side stimulus over the typed control API — the
    production command path stays on ``command()``.
    """

    def __init__(
        self,
        spec: MetricSpec[Q],
        read: Callable[[], Awaitable[Q | None]],
        set_: Callable[[Q], Awaitable[None]],
        label: str,
        *,
        kind: Kind,
        cue_kind: Kind | None = None,
        check_ref: CheckRef | None = None,
        cue: CueForm | None = None,
    ) -> None:
        Signal.__init__(self, spec, read, label, check_ref=check_ref)
        _Settable.__init__(self, set_, label, kind=kind, cue_kind=cue_kind, cue=cue)


class SettingSignal(_Settable[V]):
    """A write-only knob on a component (health, sunlight)."""
