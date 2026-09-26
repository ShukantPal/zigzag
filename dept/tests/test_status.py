import datetime as dt
import unittest

from dept.status import build_executions, detail_lines, duration_between, observed_duration


def event(kind, when, clock="vm:boot", **extra):
    return {
        "id": kind + when,
        "task_id": "task",
        "execution_id": "execution",
        "kind": kind,
        "occurred_at": when,
        "clock": clock,
        "source": "vm-department",
        **extra,
    }


class PhaseDurationTests(unittest.TestCase):
    def test_same_clock_adjacent_events_are_measured(self):
        left = event("review_wait_started", "2026-01-01T00:00:00.000Z")
        right = event("review_work_started", "2026-01-01T00:00:03.250Z")
        self.assertEqual(duration_between(left, right), dt.timedelta(seconds=3, milliseconds=250))
        self.assertEqual(observed_duration([left, right]), (dt.timedelta(seconds=3, milliseconds=250), False))

    def test_cross_clock_handoff_is_rendered_but_not_subtracted(self):
        left = event("task_dispatched", "2026-01-01T00:00:00.000Z", "vm:boot")
        right = event("relay_accepted", "2026-01-01T00:00:01.000Z", "mac:boot")
        execution = build_executions([left, right], [], relay_lost=False)[0]
        self.assertIsNone(execution.current_elapsed())
        self.assertEqual(execution.total_elapsed(), (None, True))
        self.assertTrue(any("cross-clock" in line and "not subtracted" in line for line in detail_lines(execution)))

    def test_missing_pair_means_not_observed(self):
        execution = build_executions([event("human_wait_started", "2026-01-01T00:00:00.000Z")], [])[0]
        self.assertIsNone(execution.current_elapsed())
        self.assertEqual(execution.total_elapsed(), (None, False))


if __name__ == "__main__":
    unittest.main()
