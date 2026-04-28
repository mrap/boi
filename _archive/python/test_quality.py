# test_quality.py — Tests for quality score computation library.

import sys
from pathlib import Path

# Ensure boi lib is importable
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.quality import (
    CATEGORIES,
    compute_category_scores,
    compute_effective_weights,
    compute_progress_score,
    compute_quality_score,
    detect_trend_alerts,
    format_quality_display,
    grade,
)


# ---------------------------------------------------------------------------
# Helper: build a full signals dict with a uniform score
# ---------------------------------------------------------------------------


def _all_signals(score):
    """Return a signals dict with every signal set to the given score."""
    signals = {}
    for _cat_name, (_weight, sig_ids) in CATEGORIES.items():
        for sid in sig_ids:
            signals[sid] = score
    return signals


# ---------------------------------------------------------------------------
# compute_quality_score
# ---------------------------------------------------------------------------


class TestComputeQualityScore:
    def test_all_perfect(self):
        signals = _all_signals(1.0)
        assert compute_quality_score(signals) == 1.0

    def test_all_zero(self):
        signals = _all_signals(0.0)
        assert compute_quality_score(signals) == 0.0

    def test_mixed_scores(self):
        signals = _all_signals(0.5)
        result = compute_quality_score(signals)
        assert abs(result - 0.5) < 1e-9

    def test_na_category_redistribution(self):
        """When test_quality is None, remaining weights should sum to 1.0."""
        signals = _all_signals(1.0)
        # Set all test_quality signals to None
        for sid in CATEGORIES["test_quality"][1]:
            signals[sid] = None

        result = compute_quality_score(signals)
        # With TQ removed and all others at 1.0, result should still be 1.0
        assert abs(result - 1.0) < 1e-9

    def test_na_redistribution_weights_sum_to_one(self):
        """Effective weights for active categories must sum to 1.0."""
        signals = _all_signals(0.8)
        for sid in CATEGORIES["test_quality"][1]:
            signals[sid] = None

        category_scores = compute_category_scores(signals)
        effective = compute_effective_weights(category_scores)

        assert abs(sum(effective.values()) - 1.0) < 1e-9
        assert "test_quality" not in effective

    def test_na_redistribution_proportional(self):
        """Redistributed weights maintain original proportions."""
        signals = _all_signals(0.8)
        for sid in CATEGORIES["test_quality"][1]:
            signals[sid] = None

        category_scores = compute_category_scores(signals)
        effective = compute_effective_weights(category_scores)

        # code_quality=0.35, documentation=0.15, architecture=0.25
        # sum of active = 0.75
        assert abs(effective["code_quality"] - 0.35 / 0.75) < 1e-9
        assert abs(effective["documentation"] - 0.15 / 0.75) < 1e-9
        assert abs(effective["architecture"] - 0.25 / 0.75) < 1e-9

    def test_all_na_raises(self):
        """All categories N/A should raise ValueError."""
        signals = _all_signals(None)
        try:
            compute_quality_score(signals)
            assert False, "Should have raised ValueError"
        except ValueError:
            pass

    def test_partial_signals_within_category(self):
        """Some signals None within a category. Category score uses non-null only."""
        signals = _all_signals(1.0)
        signals["CQ-1"] = None
        signals["CQ-2"] = None
        # CQ-3 through CQ-6 are still 1.0 -> code_quality = 1.0
        result = compute_quality_score(signals)
        assert abs(result - 1.0) < 1e-9


# ---------------------------------------------------------------------------
# compute_category_scores
# ---------------------------------------------------------------------------


class TestComputeCategoryScores:
    def test_uniform_scores(self):
        signals = _all_signals(0.7)
        cats = compute_category_scores(signals)
        for name in CATEGORIES:
            assert abs(cats[name] - 0.7) < 1e-9

    def test_all_none_category(self):
        signals = _all_signals(1.0)
        for sid in CATEGORIES["documentation"][1]:
            signals[sid] = None
        cats = compute_category_scores(signals)
        assert cats["documentation"] is None
        assert cats["code_quality"] is not None

    def test_mixed_within_category(self):
        signals = _all_signals(0.0)
        # Set half of code_quality signals to 1.0
        cq_signals = CATEGORIES["code_quality"][1]
        half = len(cq_signals) // 2
        for sid in cq_signals[:half]:
            signals[sid] = 1.0
        cats = compute_category_scores(signals)
        expected = half / len(cq_signals)
        assert abs(cats["code_quality"] - expected) < 1e-9


# ---------------------------------------------------------------------------
# compute_progress_score
# ---------------------------------------------------------------------------


class TestComputeProgressScore:
    def test_full_completion_zero_quality(self):
        assert abs(compute_progress_score(1.0, 0.0) - 0.5) < 1e-9

    def test_full_completion_full_quality(self):
        assert abs(compute_progress_score(1.0, 1.0) - 1.0) < 1e-9

    def test_half_completion_full_quality(self):
        assert abs(compute_progress_score(0.5, 1.0) - 0.5) < 1e-9

    def test_zero_completion(self):
        assert abs(compute_progress_score(0.0, 1.0) - 0.0) < 1e-9

    def test_half_completion_half_quality(self):
        # 0.5 * (0.5 + 0.5 * 0.5) = 0.5 * 0.75 = 0.375
        assert abs(compute_progress_score(0.5, 0.5) - 0.375) < 1e-9


# ---------------------------------------------------------------------------
# grade
# ---------------------------------------------------------------------------


class TestGrade:
    def test_grade_a(self):
        assert grade(0.90) == "A"
        assert grade(0.95) == "A"
        assert grade(1.0) == "A"

    def test_grade_b(self):
        assert grade(0.80) == "B"
        assert grade(0.85) == "B"
        assert grade(0.89) == "B"

    def test_grade_c(self):
        assert grade(0.70) == "C"
        assert grade(0.75) == "C"
        assert grade(0.79) == "C"

    def test_grade_d(self):
        assert grade(0.50) == "D"
        assert grade(0.60) == "D"
        assert grade(0.69) == "D"

    def test_grade_f(self):
        assert grade(0.49) == "F"
        assert grade(0.0) == "F"
        assert grade(0.45) == "F"

    def test_boundary_values(self):
        assert grade(0.90) == "A"
        assert grade(0.8999) == "B"
        assert grade(0.80) == "B"
        assert grade(0.7999) == "C"
        assert grade(0.70) == "C"
        assert grade(0.6999) == "D"
        assert grade(0.50) == "D"
        assert grade(0.4999) == "F"


# ---------------------------------------------------------------------------
# detect_trend_alerts
# ---------------------------------------------------------------------------


class TestDetectTrendAlerts:
    def test_declining_quality_three_drops(self):
        history = [0.80, 0.75, 0.60]
        alerts = detect_trend_alerts(history)
        types = [a["type"] for a in alerts]
        assert "declining_quality" in types

    def test_declining_quality_four_drops(self):
        history = [0.90, 0.85, 0.80, 0.70, 0.60]
        alerts = detect_trend_alerts(history)
        types = [a["type"] for a in alerts]
        assert "declining_quality" in types

    def test_no_decline(self):
        history = [0.80, 0.82, 0.85]
        alerts = detect_trend_alerts(history)
        types = [a["type"] for a in alerts]
        assert "declining_quality" not in types

    def test_small_decline_no_alert(self):
        # Drops but total < 0.10
        history = [0.80, 0.79, 0.78, 0.77]
        alerts = detect_trend_alerts(history)
        declining_alerts = [a for a in alerts if a["type"] == "declining_quality"]
        # Total drop = 0.03 (3 consecutive), which is < 0.10
        assert len(declining_alerts) == 0

    def test_quality_plateau(self):
        history = [0.70, 0.71, 0.70, 0.69, 0.70]
        alerts = detect_trend_alerts(history)
        types = [a["type"] for a in alerts]
        assert "quality_plateau" in types

    def test_no_plateau_above_threshold(self):
        # Plateau at 0.85 (above 0.80) should NOT alert
        history = [0.85, 0.85, 0.85, 0.85, 0.85]
        alerts = detect_trend_alerts(history)
        types = [a["type"] for a in alerts]
        assert "quality_plateau" not in types

    def test_low_quality_alert(self):
        history = [0.80, 0.60, 0.40]
        alerts = detect_trend_alerts(history)
        types = [a["type"] for a in alerts]
        assert "low_quality" in types

    def test_no_low_quality_all_above(self):
        history = [0.80, 0.75, 0.70]
        alerts = detect_trend_alerts(history)
        types = [a["type"] for a in alerts]
        assert "low_quality" not in types

    def test_single_entry(self):
        alerts = detect_trend_alerts([0.80])
        assert alerts == []

    def test_empty_history(self):
        alerts = detect_trend_alerts([])
        assert alerts == []

    def test_two_entries_no_plateau(self):
        # Not enough data for plateau (needs 5)
        alerts = detect_trend_alerts([0.70, 0.70])
        plateau_alerts = [a for a in alerts if a["type"] == "quality_plateau"]
        assert len(plateau_alerts) == 0


# ---------------------------------------------------------------------------
# format_quality_display
# ---------------------------------------------------------------------------


class TestFormatQualityDisplay:
    def test_basic_format(self):
        assert format_quality_display(0.78, "B") == "B (0.78)"

    def test_perfect_score(self):
        assert format_quality_display(1.0, "A") == "A (1.00)"

    def test_zero_score(self):
        assert format_quality_display(0.0, "F") == "F (0.00)"

    def test_rounding(self):
        assert format_quality_display(0.777, "C") == "C (0.78)"


# ---------------------------------------------------------------------------
# compute_effective_weights
# ---------------------------------------------------------------------------


class TestComputeEffectiveWeights:
    def test_all_active(self):
        category_scores = {
            "code_quality": 0.8,
            "test_quality": 0.7,
            "documentation": 0.9,
            "architecture": 0.6,
        }
        weights = compute_effective_weights(category_scores)
        assert abs(sum(weights.values()) - 1.0) < 1e-9
        # All active, weights should equal original
        assert abs(weights["code_quality"] - 0.35) < 1e-9
        assert abs(weights["test_quality"] - 0.25) < 1e-9
        assert abs(weights["documentation"] - 0.15) < 1e-9
        assert abs(weights["architecture"] - 0.25) < 1e-9

    def test_one_na(self):
        category_scores = {
            "code_quality": 0.8,
            "test_quality": None,
            "documentation": 0.9,
            "architecture": 0.6,
        }
        weights = compute_effective_weights(category_scores)
        assert "test_quality" not in weights
        assert abs(sum(weights.values()) - 1.0) < 1e-9

    def test_all_na(self):
        category_scores = {
            "code_quality": None,
            "test_quality": None,
            "documentation": None,
            "architecture": None,
        }
        weights = compute_effective_weights(category_scores)
        assert weights == {}
