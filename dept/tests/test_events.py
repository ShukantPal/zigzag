import json
import tempfile
import unittest
from pathlib import Path
from urllib.error import URLError

from dept.events import post_event, transition_event


class FakeResponse:
    def __enter__(self):
        return self

    def __exit__(self, *args):
        return None

    def read(self):
        return b"{}"


class EventEmissionTests(unittest.TestCase):
    def test_transition_emits_schema_v1_department_envelope(self):
        event = transition_event(
            event_id="execution:review_wait_started:1",
            task_id="task",
            execution_id="execution",
            kind="review_wait_started",
            occurred_at="2026-01-01T00:00:00.000Z",
            clock="vm:boot",
        )
        self.assertEqual(
            event,
            {
                "schema_version": 1,
                "id": "execution:review_wait_started:1",
                "task_id": "task",
                "execution_id": "execution",
                "kind": "review_wait_started",
                "source": "vm-department",
                "occurred_at": "2026-01-01T00:00:00.000Z",
                "clock": "vm:boot",
                "payload": {},
            },
        )

    def test_transition_rejects_incomplete_or_unsupported_envelopes(self):
        with self.assertRaises(ValueError):
            transition_event(event_id="", task_id="task", execution_id="execution", kind="review_wait_started")
        with self.assertRaises(ValueError):
            transition_event(event_id="event", task_id="task", execution_id="execution", kind="not_a_transition")

    def test_retry_reuses_the_exact_id_and_body(self):
        event = transition_event(
            event_id="execution:human_wait_ended:1",
            task_id="task",
            execution_id="execution",
            kind="human_wait_ended",
            occurred_at="2026-01-01T00:00:00.000Z",
            clock="vm:boot",
        )
        requests = []

        def opener(request, timeout):
            requests.append(request)
            if len(requests) == 1:
                raise URLError("temporary")
            return FakeResponse()

        with tempfile.TemporaryDirectory() as directory:
            token = Path(directory) / "token"
            token.write_text("secret")
            post_event(event, url="http://127.0.0.1:8765", token_file=token, opener=opener)
        self.assertEqual(len(requests), 2)
        self.assertEqual(requests[0].data, requests[1].data)
        self.assertEqual(json.loads(requests[1].data)["id"], "execution:human_wait_ended:1")


if __name__ == "__main__":
    unittest.main()
