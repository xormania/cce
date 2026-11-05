"""Offline provider-boundary regressions using typed choices and bounded replies."""
import copy
import importlib.util
import io
import json
from pathlib import Path
import socket
import unittest
from unittest import mock
import urllib.error


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("jev_client", ROOT / "tools/ci/jev_client.py")
client = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(client)


def packet():
    return {
        "subject": "a" * 16,
        "source": {"repository": "xormania/cce", "head_sha": "b" * 40, "base_sha": "c" * 40},
        "excerpts": [{"id": "e1", "text": "The claim is partial and its incomplete test is acknowledged."},
                     {"id": "e2", "text": "A held reader is checked against the expected original bytes."}],
        "questions": ["proof", "witness"],
    }


def response(value=None, choices=None):
    payload = client.payload_for(value or packet())
    choices = choices or {"proof": "supported", "witness": "semantic_behavior",
                          "support_1": "e1", "support_2": "e2"}
    return {"model": client.MODEL,
            "answers": {name: {"type": "choice", "choice": choices.get(name, "insufficient_context")}
                        for name in payload["questions"]},
            "usage": {"input_tokens": 500, "output_tokens": 80, "total_tokens": 580}}


class Handle(io.BytesIO):
    status = 200

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        self.close()


class Opener:
    def __init__(self, body=None, error=None):
        self.body = body if isinstance(body, bytes) else json.dumps(body or response()).encode()
        self.error = error
        self.calls = []

    def open(self, request, timeout):
        self.calls.append((request, timeout))
        if self.error:
            raise self.error
        return Handle(self.body)


class PayloadTests(unittest.TestCase):
    def test_only_requested_atomic_questions_and_evidence_supports(self):
        value = packet()
        payload = client.payload_for(value)
        self.assertEqual(payload["model"], "jev-1.14.0")
        self.assertEqual(set(payload["questions"]), {"proof", "witness", "support_1", "support_2"})
        self.assertEqual(set(payload["questions"]["support_1"]["criteria"]), {"e1", "e2", "none"})
        self.assertEqual(payload["state"]["excerpts"], value["excerpts"])
        self.assertIn("untrusted data", payload["questions"]["proof"]["instructions"])

    def test_opaque_subject_and_public_source_are_validated(self):
        for location, bad in (("subject", "private-label"), ("source", {"repository": "hidden-data"})):
            value = packet()
            value[location] = bad
            with self.subTest(location=location), self.assertRaises(ValueError):
                client.payload_for(value)

    def test_excerpts_have_local_safe_ids(self):
        for eid in ("none", "private-record", "e0", "e10000"):
            value = packet()
            value["excerpts"][0]["id"] = eid
            with self.subTest(eid=eid), self.assertRaises(ValueError):
                client.payload_for(value)

    def test_duplicate_excerpt_or_question_is_refused(self):
        for change in (lambda value: value["excerpts"].append(value["excerpts"][0]),
                       lambda value: value["questions"].append("proof")):
            value = packet()
            change(value)
            with self.assertRaises(ValueError):
                client.payload_for(value)

    def test_byte_budget_includes_questions(self):
        value = packet()
        value["excerpts"] = [{"id": f"e{n}", "text": "x" * 9_000} for n in range(1, 7)]
        with self.assertRaises(ValueError):
            client.payload_for(value)

    def test_utf8_byte_bound_is_not_character_count(self):
        value = packet()
        value["excerpts"][0]["text"] = "é" * 6_000
        with self.assertRaises(ValueError):
            client.payload_for(value)


class ResponseTests(unittest.TestCase):
    def setUp(self):
        self.payload = client.payload_for(packet())

    def test_valid_typed_answers_with_optional_metadata(self):
        value = response()
        value["answers"]["proof"].update(confidence=.9, probabilities={
            "supported": .9, "overstated": .05, "contradicted": .03, "insufficient_context": .02})
        clean = client.validate_response(value, self.payload)
        self.assertFalse(clean["abstained"])
        self.assertEqual(clean["supporting_excerpt_ids"], ["e1", "e2"])
        self.assertEqual(clean["answers"]["proof"]["confidence"], .9)

    def test_one_support_is_sufficient(self):
        value = response()
        value["answers"]["support_2"]["choice"] = "none"
        self.assertEqual(client.validate_response(value, self.payload)["supporting_excerpt_ids"], ["e1"])

    def test_duplicate_support_is_rejected_instead_of_repaired(self):
        value = response()
        value["answers"]["support_2"]["choice"] = "e1"
        with self.assertRaises(ValueError):
            client.validate_response(value, self.payload)

    def test_unsupported_nonabstention_is_rejected(self):
        value = response()
        for name in ("support_1", "support_2"):
            value["answers"][name]["choice"] = "none"
        with self.assertRaises(ValueError):
            client.validate_response(value, self.payload)

    def test_full_abstention_allows_no_support(self):
        value = response(choices={"proof": "insufficient_context", "witness": "insufficient_context",
                                  "support_1": "none", "support_2": "none"})
        self.assertTrue(client.validate_response(value, self.payload)["abstained"])

    def test_wrong_model_inventory_choice_and_invented_reference_are_rejected(self):
        changes = [lambda value: value.update(model="other-model"),
                   lambda value: value["answers"].pop("proof"),
                   lambda value: value["answers"].update(extra={"type": "choice", "choice": "hidden"}),
                   lambda value: value["answers"]["proof"].update(choice="arbitrary provider prose"),
                   lambda value: value["answers"]["support_1"].update(choice="e9")]
        for change in changes:
            value = response()
            change(value)
            with self.subTest(change=change), self.assertRaises(ValueError):
                client.validate_response(value, self.payload)

    def test_numeric_metadata_is_finite_bounded_and_not_boolean(self):
        values = [-.1, 1.1, float("inf"), float("nan"), True, "0.5", 10**1000]
        for confidence in values:
            value = response()
            value["answers"]["proof"]["confidence"] = confidence
            with self.subTest(confidence=confidence), self.assertRaises(ValueError):
                client.validate_response(value, self.payload)
        for probabilities in ({"supported": 1}, {key: 1 for key in client.CRITERIA["proof"]}):
            value = response()
            value["answers"]["proof"]["probabilities"] = probabilities
            with self.assertRaises(ValueError):
                client.validate_response(value, self.payload)


class TransportTests(unittest.TestCase):
    def test_missing_credentials_do_not_make_a_request(self):
        opener = Opener()
        result = client.diagnose(packet(), opener=opener)
        self.assertEqual(result["status"], "missing_credentials")
        self.assertFalse(result["provider_request_attempted"])
        self.assertEqual(result["provider_request_attempts"], 0)
        self.assertEqual(opener.calls, [])

    def test_exact_endpoint_timeout_and_single_attempt_are_recorded(self):
        opener = Opener()
        result = client.diagnose(packet(), key="test-key", opener=opener)
        self.assertEqual(result["status"], "diagnosed")
        self.assertEqual(result["observed_model"], client.MODEL)
        self.assertEqual(result["usage"]["total_tokens"], 580)
        self.assertEqual(result["provider_request_attempts"], 1)
        request, timeout = opener.calls[0]
        self.assertEqual(request.full_url, client.ENDPOINT)
        self.assertEqual(timeout, 15)
        self.assertEqual(request.get_header("Authorization"), "Bearer test-key")
        self.assertEqual(len(result["request_sha256"]), 64)
        self.assertEqual(len(result["response_sha256"]), 64)
        self.assertFalse(result["full_response_retained"])

    def test_abstention_is_distinct_from_diagnosed(self):
        body = response(choices={"proof": "insufficient_context", "witness": "insufficient_context",
                                 "support_1": "none", "support_2": "none"})
        result = client.diagnose(packet(), key="key", opener=Opener(body))
        self.assertEqual(result["status"], "abstained")

    def test_active_credential_is_redacted_from_evidence_before_request(self):
        value = packet()
        value["excerpts"][0]["text"] = "accidentally captured secret-provider-token"
        opener = Opener()
        client.diagnose(value, key="secret-provider-token", opener=opener)
        request, _timeout = opener.calls[0]
        self.assertNotIn(b"secret-provider-token", request.data)
        self.assertIn(b"[REDACTED]", request.data)
        self.assertIn("secret-provider-token", value["excerpts"][0]["text"])

    def test_timeout_or_os_failure_does_not_retry_or_echo_error(self):
        errors = [(TimeoutError("sensitive detail"), "timeout"),
                  (urllib.error.URLError(socket.timeout("sensitive detail")), "timeout"),
                  (OSError("sensitive detail"), "provider_failure")]
        for error, status in errors:
            opener = Opener(error=error)
            result = client.diagnose(packet(), key="key", opener=opener)
            with self.subTest(status=status):
                self.assertEqual(result["status"], status)
                self.assertEqual(len(opener.calls), 1)
                self.assertNotIn("sensitive detail", json.dumps(result))

    def test_http_error_body_is_not_returned(self):
        error = urllib.error.HTTPError(client.ENDPOINT, 429, "sensitive message", {}, io.BytesIO(b"sensitive body"))
        result = client.diagnose(packet(), key="key", opener=Opener(error=error))
        self.assertEqual(result["status"], "provider_failure")
        self.assertNotIn("sensitive", json.dumps(result))
        self.assertIsNotNone(result["response_sha256"])

    def test_oversized_or_non_json_response_is_rejected(self):
        for raw in (b"x" * (client.MAX_RESPONSE + 1), b"not-json", b'[{"too":"nested"}]'):
            result = client.diagnose(packet(), key="key", opener=Opener(raw))
            with self.subTest(size=len(raw)):
                self.assertEqual(result["status"], "malformed_response")

    def test_duplicate_json_keys_and_nonfinite_values_are_rejected(self):
        raws = [b'{"model":"jev-1.14.0","model":"jev-1.14.0"}',
                b'{"model":"jev-1.14.0","value":NaN}',
                b'{"model":"jev-1.14.0","value":1e10000}']
        for raw in raws:
            with self.subTest(raw=raw):
                result = client.diagnose(packet(), key="key", opener=Opener(raw))
                self.assertEqual(result["status"], "malformed_response")

    def test_private_evidence_provider_echo_and_errors_never_escape(self):
        marker = "DO_NOT_PUBLISH_9f582b"
        value = packet()
        value["excerpts"][0]["text"] = marker
        body = response(value)
        body["private_echo"] = marker
        body["answers"]["proof"]["explanation"] = marker
        body["usage"][marker] = 1
        result = client.diagnose(value, key=marker, opener=Opener(body))
        self.assertNotIn(marker, json.dumps(result))
        self.assertEqual(result["status"], "diagnosed")
        body["answers"]["proof"]["choice"] = marker
        result = client.diagnose(value, key="key", opener=Opener(body))
        self.assertNotIn(marker, json.dumps(result))
        self.assertEqual(result["status"], "malformed_response")

    def test_redirect_handler_refuses_redirect(self):
        self.assertIsNone(client._NoRedirect().redirect_request(None, None, 302, "", {}, "https://example.invalid"))


class DiagnosticTests(unittest.TestCase):
    def diagnostic(self, body):
        result = client.diagnose(packet(), key="test-key", opener=Opener(body))
        self.assertEqual(result["status"], "malformed_response")
        return result["diagnostic"]

    def test_http_failures_identify_auth_rate_limit_and_other_errors_without_text(self):
        marker = "private-http-error-marker"
        for status, reason in ((401, "authentication_failed"), (403, "forbidden"),
                               (429, "rate_limited"), (302, "redirect_refused"), (503, "http_error")):
            with self.subTest(status=status):
                error = urllib.error.HTTPError(client.ENDPOINT, status, marker,
                                               {"Location": marker}, io.BytesIO(marker.encode()))
                opener = Opener(error=error)
                result = client.diagnose(packet(), key=marker, opener=opener)
                self.assertEqual(result["status"], "provider_failure")
                diagnostic = result["diagnostic"]
                self.assertEqual((diagnostic["stage"], diagnostic["reason"]), ("http", reason))
                self.assertEqual(diagnostic["http_status"], status)
                self.assertEqual(diagnostic["response_bytes_read"], len(marker))
                self.assertFalse(diagnostic["response_truncated"])
                self.assertEqual(len(opener.calls), 1)
                self.assertNotIn(marker, json.dumps(result))

    def test_transport_reasons_do_not_invent_an_http_response(self):
        for error, reason in ((TimeoutError("private detail"), "request_timeout"),
                              (urllib.error.URLError(socket.timeout("private detail")), "request_timeout"),
                              (urllib.error.URLError("private detail"), "network_error"),
                              (OSError("private detail"), "io_error")):
            with self.subTest(reason=reason):
                result = client.diagnose(packet(), key="key", opener=Opener(error=error))
                diagnostic = result["diagnostic"]
                self.assertEqual((diagnostic["stage"], diagnostic["reason"]), ("transport", reason))
                self.assertIsNone(diagnostic["http_status"])
                self.assertIsNone(diagnostic["response_bytes_read"])
                self.assertNotIn("private detail", json.dumps(result))

    def test_response_read_timeout_preserves_observed_http_status(self):
        class SlowHandle(Handle):
            def read(self, _limit):
                raise TimeoutError("private detail")
        class SlowOpener:
            def open(self, *_args, **_kwargs):
                return SlowHandle()
        result = client.diagnose(packet(), key="key", opener=SlowOpener())
        self.assertEqual(result["diagnostic"]["reason"], "response_read_timeout")
        self.assertEqual(result["diagnostic"]["http_status"], 200)
        self.assertIsNone(result["diagnostic"]["response_bytes_read"])

    def test_oversized_response_reports_only_observed_prefix_length(self):
        result = client.diagnose(packet(), key="key", opener=Opener(b"x" * (client.MAX_RESPONSE + 100)))
        diagnostic = result["diagnostic"]
        self.assertEqual((diagnostic["stage"], diagnostic["reason"]), ("body", "response_too_large"))
        self.assertEqual(diagnostic["response_bytes_read"], client.MAX_RESPONSE + 1)
        self.assertTrue(diagnostic["response_truncated"])
        self.assertIsNone(result["response_sha256"])

    def test_decode_reasons_are_distinct_from_response_schema(self):
        for raw, reason in ((b"not-json", "json_invalid"), (b"\xff", "encoding_invalid"),
                            (b'{"x":1,"x":2}', "json_duplicate_field"),
                            (b'{"x":NaN}', "json_nonfinite_number"),
                            (b'{"x":1e10000}', "json_nonfinite_number")):
            with self.subTest(reason=reason):
                diagnostic = self.diagnostic(raw)
                self.assertEqual((diagnostic["stage"], diagnostic["reason"]), ("json", reason))
                self.assertNotIn("response_type", diagnostic)
        with mock.patch.object(client.json, "loads", side_effect=RecursionError("private detail")):
            diagnostic = self.diagnostic(b"[]")
            self.assertEqual((diagnostic["stage"], diagnostic["reason"]), ("json", "json_nesting_limit"))
            self.assertNotIn("private detail", json.dumps(diagnostic))
        for raw, kind in ((b"[]", "array"), (b"null", "null"), (b"true", "boolean")):
            diagnostic = self.diagnostic(raw)
            self.assertEqual(diagnostic["reason"], "response_not_object")
            self.assertEqual(diagnostic["response_type"], kind)

    def test_model_and_inventory_rejections_are_separate(self):
        cases = [
            (lambda value: value.pop("model"), "model_missing"),
            (lambda value: value.update(model=None), "model_type_invalid"),
            (lambda value: value.update(model="private-model-name"), "model_mismatch"),
            (lambda value: value.pop("answers"), "answers_missing"),
            (lambda value: value.update(answers=[]), "answers_not_object"),
            (lambda value: value["answers"].pop("proof"), "answer_inventory_mismatch"),
        ]
        for mutate, reason in cases:
            with self.subTest(reason=reason):
                value = response()
                mutate(value)
                diagnostic = self.diagnostic(value)
                self.assertEqual((diagnostic["stage"], diagnostic["reason"]), ("schema", reason))
                self.assertNotIn("private-model-name", json.dumps(diagnostic))

    def test_answer_and_numeric_rejections_identify_only_the_known_question(self):
        cases = [(None, "answer_not_object"),
                 ({"type": "private-type", "choice": "supported"}, "answer_type_invalid"),
                 ({"type": "choice", "choice": "private-choice"}, "choice_not_allowed"),
                 ({"type": "choice", "choice": "supported", "confidence": True}, "confidence_invalid"),
                 ({"type": "choice", "choice": "supported", "probabilities": {}}, "probability_inventory_mismatch")]
        for answer, reason in cases:
            with self.subTest(reason=reason):
                value = response()
                value["answers"]["proof"] = answer
                diagnostic = self.diagnostic(value)
                self.assertEqual(diagnostic["reason"], reason)
                self.assertEqual(diagnostic["question"], "proof")
                self.assertNotIn("private-", json.dumps(diagnostic))
        for probabilities, reason in (({name: True for name in client.CRITERIA["proof"]}, "probability_value_invalid"),
                                      ({name: .5 for name in client.CRITERIA["proof"]}, "probability_total_invalid")):
            value = response()
            value["answers"]["proof"]["probabilities"] = probabilities
            self.assertEqual(self.diagnostic(value)["reason"], reason)

    def test_local_support_policy_rejection_is_not_json_failure(self):
        for first, second, reason in (("e1", "e1", "duplicate_support"),
                                      ("none", "none", "unsupported_diagnosis")):
            with self.subTest(reason=reason):
                value = response()
                value["answers"]["support_1"]["choice"] = first
                value["answers"]["support_2"]["choice"] = second
                diagnostic = self.diagnostic(value)
                self.assertEqual((diagnostic["stage"], diagnostic["reason"]), ("policy", reason))
                self.assertEqual(diagnostic["http_status"], 200)

    def test_shape_metadata_contains_counts_and_types_without_unknown_names(self):
        marker = "private-schema-marker"
        value = response()
        value[marker] = marker
        value["error"] = {marker: marker}
        value["answers"].pop("proof")
        value["answers"][marker] = {"choice": marker}
        diagnostic = self.diagnostic(value)
        self.assertEqual(diagnostic["known_field_types"],
                         {"model": "string", "answers": "object", "usage": "object", "error": "object"})
        self.assertEqual(diagnostic["unknown_field_count"], 1)
        self.assertEqual(diagnostic["expected_answer_count"], 4)
        self.assertEqual(diagnostic["answer_count"], 4)
        self.assertEqual(diagnostic["missing_answer_count"], 1)
        self.assertEqual(diagnostic["unexpected_answer_count"], 1)
        self.assertNotIn(marker, json.dumps(diagnostic))

    def test_public_diagnostic_projection_rejects_unbounded_or_unknown_fields(self):
        good = self.diagnostic(b"not-json")
        self.assertEqual(client.public_diagnostic(good), good)
        for changed in ({"stage": "private-stage"}, {"reason": "private-reason"},
                        {"stage": "http"}, {"http_status": True}, {"http_status": 999},
                        {"response_bytes_read": client.MAX_RESPONSE + 2},
                        {"response_truncated": 1}, {"unknown_field_count": -1},
                        {"expected_answer_count": 8}, {"question": "private-question"},
                        {"response_type": "private-type"},
                        {"known_field_types": {"private-key": "string"}},
                        {"known_field_types": {"answers": "private-type"}},
                        {"private-extra": "private-value"}):
            with self.subTest(changed=changed):
                self.assertIsNone(client.public_diagnostic(dict(good, **changed)))

    def test_success_and_missing_credentials_keep_existing_record_shapes(self):
        for body in (response(), response(choices={"proof": "insufficient_context", "witness": "insufficient_context",
                                                  "support_1": "none", "support_2": "none"})):
            result = client.diagnose(packet(), key="key", opener=Opener(body))
            self.assertNotIn("diagnostic", result)
        opener = Opener()
        self.assertNotIn("diagnostic", client.diagnose(packet(), opener=opener))
        self.assertEqual(opener.calls, [])


if __name__ == "__main__":
    unittest.main()
