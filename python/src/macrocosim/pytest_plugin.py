"""pytest plugin: a ``macrocosim`` fixture that launches and tears down a
Site for each test.

Auto-loaded via the ``pytest11`` entry point once the package is installed.
Provide a ``macrocosim_config`` fixture (a :class:`macrocosim.build.Microgrid`
or a path to a ``.lisp`` file) and depend on ``macrocosim``:

    import macrocosim as mc
    import pytest
    from frequenz.quantities import Power

    @pytest.fixture
    def macrocosim_config():
        return mc.Microgrid(id=1, topology=mc.grid(id=1,
            successors=[mc.meter(id=2, power=Power.from_watts(7000))]))

    async def test_grid_holds(macrocosim):
        await macrocosim.expect.grid_power(
            approx=Power.from_kilowatts(7), tol=Power.from_watts(500))

The ``expect`` assertions are ``async``, so tests that await them must run
under ``pytest-asyncio`` (installed with the ``grpc`` extra) with
``asyncio_mode = "auto"`` in your pyproject/pytest config, or be marked
``@pytest.mark.asyncio`` individually. Without it pytest collects an
``async def`` test but never awaits it — the assertion silently never runs
and the test passes green.

The binary is found via ``MACROCOSIM_BIN`` / ``$PATH`` (or pass one through
a ``macrocosim_bin`` fixture).

The plugin needs pytest 7.4 or newer and pluggy 1.2 or newer (``StashKey``,
new-style hook wrappers); older pytests fail to load it at the ``pytest11``
entry point.
"""

from __future__ import annotations

from collections.abc import Generator, Iterator

import pytest

import macrocosim as mc
from macrocosim.build import LaunchConfig


def pytest_configure(config: pytest.Config) -> None:
    config.addinivalue_line(
        "markers",
        "macrocosim_scenario(name): run the named registered scenario after a "
        "passing test body and fail on any failed check, or if none ran. The "
        "failure is reported as a teardown error on the test.",
    )


_CALL_PASSED = pytest.StashKey[bool]()


@pytest.hookimpl(wrapper=True)
def pytest_runtest_makereport(
    item: pytest.Item, call: pytest.CallInfo[None]
) -> Generator[None, pytest.TestReport, pytest.TestReport]:
    """Record whether the test body itself passed, for the fixture to read."""
    report = yield
    if report.when == "call":
        item.stash[_CALL_PASSED] = report.passed
    return report


@pytest.fixture
def macrocosim_bin() -> str | None:
    """Override to point at a specific binary; defaults to env/PATH lookup."""
    return None


@pytest.fixture
def macrocosim_config() -> LaunchConfig:
    raise RuntimeError(
        "define a `macrocosim_config` fixture returning a macrocosim.Microgrid "
        "or a path to a .lisp config"
    )


@pytest.fixture
def macrocosim(
    macrocosim_config: LaunchConfig,
    macrocosim_bin: str | None,
    request: pytest.FixtureRequest,
) -> Iterator[mc.Site]:
    """A freshly launched Site per test, torn down afterwards.

    If the test is marked ``@pytest.mark.macrocosim_scenario("name")``, the
    named scenario is run and gated after a passing test body. A failed check
    is reported as a teardown error on the test, which is what pytest reports
    for a fixture that raises after its yield.
    """
    with mc.launch(macrocosim_config, bin=macrocosim_bin) as site:
        yield site
        # Only a body that ran and passed leaves something worth gating.
        # Skipping saves the scenario's full length on a red test, and a
        # body that was skipped, xfailed, or never ran because another
        # fixture failed in setup has no result for the scenario to confirm.
        if not request.node.stash.get(_CALL_PASSED, False):
            return
        marker = request.node.get_closest_marker("macrocosim_scenario")
        if marker is not None:
            if not marker.args:
                raise pytest.UsageError(
                    "@pytest.mark.macrocosim_scenario needs the scenario name, "
                    'e.g. @pytest.mark.macrocosim_scenario("hold-load")'
                )
            site.scenario(str(marker.args[0])).run(wait=True).assert_passed()
