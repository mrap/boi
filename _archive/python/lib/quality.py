# quality.py — Quality score computation for BOI specs.
#
# Computes quality scores from per-signal data produced by the
# quality-scoring prompt template. Provides progress scoring,
# letter grading, trend detection, and display formatting.

from typing import Optional


# Category definitions: name -> (weight, signal_ids)
CATEGORIES: dict[str, tuple[float, list[str]]] = {
    "code_quality": (0.35, ["CQ-1", "CQ-2", "CQ-3", "CQ-4", "CQ-5", "CQ-6"]),
    "test_quality": (0.25, ["TQ-1", "TQ-2", "TQ-3", "TQ-4", "TQ-5"]),
    "documentation": (0.15, ["DOC-1", "DOC-2", "DOC-3"]),
    "architecture": (0.25, ["ARCH-1", "ARCH-2", "ARCH-3", "ARCH-4"]),
}

# Grade thresholds (inclusive lower bound)
_GRADE_THRESHOLDS: list[tuple[float, str]] = [
    (0.90, "A"),
    (0.80, "B"),
    (0.70, "C"),
    (0.50, "D"),
    (0.00, "F"),
]


def compute_category_scores(
    signals: dict[str, Optional[float]],
) -> dict[str, Optional[float]]:
    """Compute per-category scores from per-signal scores.

    Each category score is the average of its non-null signal scores.
    If all signals in a category are null, the category score is None.

    Args:
        signals: mapping of signal ID (e.g. "CQ-1") to score (0.0-1.0) or None.

    Returns:
        mapping of category name to score (0.0-1.0) or None.
    """
    result: dict[str, Optional[float]] = {}
    for cat_name, (_weight, signal_ids) in CATEGORIES.items():
        scores = [
            signals[sid]
            for sid in signal_ids
            if sid in signals and signals[sid] is not None
        ]
        if scores:
            result[cat_name] = sum(scores) / len(scores)
        else:
            result[cat_name] = None
    return result


def compute_quality_score(signals: dict[str, Optional[float]]) -> float:
    """Apply category weights to per-signal scores. Return 0.0-1.0.

    Category weights: code_quality=0.35, test_quality=0.25,
    documentation=0.15, architecture=0.25.

    N/A categories have their weight redistributed proportionally
    to the remaining active categories.

    Args:
        signals: mapping of signal ID to score (0.0-1.0) or None.

    Returns:
        overall quality score between 0.0 and 1.0.

    Raises:
        ValueError: if no categories have scores (all N/A).
    """
    category_scores = compute_category_scores(signals)

    # Collect active categories (non-None scores)
    active: list[tuple[str, float]] = []
    for cat_name, score in category_scores.items():
        if score is not None:
            active.append((cat_name, score))

    if not active:
        raise ValueError("All categories are N/A. Cannot compute quality score.")

    # Compute effective weights (redistribute N/A weight)
    active_weight_sum = sum(CATEGORIES[name][0] for name, _ in active)
    overall = 0.0
    for cat_name, cat_score in active:
        original_weight = CATEGORIES[cat_name][0]
        effective_weight = original_weight / active_weight_sum
        overall += cat_score * effective_weight

    return overall


def compute_progress_score(completion: float, quality: float) -> float:
    """Compute progress as completion weighted by quality.

    Formula: progress = completion * (0.5 + 0.5 * quality)

    At quality=0, progress is half of completion (work done but poorly).
    At quality=1, progress equals completion (full credit).

    Args:
        completion: task completion ratio (0.0-1.0).
        quality: overall quality score (0.0-1.0).

    Returns:
        progress score between 0.0 and 1.0.
    """
    return completion * (0.5 + 0.5 * quality)


def grade(progress_score: float) -> str:
    """Return letter grade based on score thresholds.

    A: >= 0.90
    B: >= 0.80
    C: >= 0.70
    D: >= 0.50
    F: < 0.50
    """
    for threshold, letter in _GRADE_THRESHOLDS:
        if progress_score >= threshold:
            return letter
    return "F"


def detect_trend_alerts(quality_history: list[float]) -> list[dict[str, str]]:
    """Check for concerning quality trends.

    Detects:
    - Declining quality: 3+ consecutive drops totaling > 0.10.
    - Quality plateau: last 5+ scores vary by < 0.05 and are below 0.80.
    - Verify substance: any score below 0.50 in recent history.

    Args:
        quality_history: list of quality scores in chronological order.

    Returns:
        list of alert dicts with 'type' and 'message' keys.
    """
    alerts: list[dict[str, str]] = []

    if len(quality_history) < 2:
        return alerts

    # Declining quality: 3+ consecutive drops
    if len(quality_history) >= 3:
        consecutive_drops = 0
        total_drop = 0.0
        for i in range(len(quality_history) - 1, 0, -1):
            diff = quality_history[i] - quality_history[i - 1]
            if diff < 0:
                consecutive_drops += 1
                total_drop += abs(diff)
            else:
                break

        if consecutive_drops >= 3 and total_drop > 0.10:
            alerts.append(
                {
                    "type": "declining_quality",
                    "message": (
                        f"Quality declining: {consecutive_drops} consecutive drops, "
                        f"total decline of {total_drop:.2f}"
                    ),
                }
            )
        elif consecutive_drops >= 2 and total_drop > 0.10:
            alerts.append(
                {
                    "type": "declining_quality",
                    "message": (
                        f"Quality dropping: declined {total_drop:.2f} "
                        f"over last {consecutive_drops} iterations"
                    ),
                }
            )

    # Quality plateau: last 5+ scores cluster below 0.80
    if len(quality_history) >= 5:
        recent = quality_history[-5:]
        spread = max(recent) - min(recent)
        avg = sum(recent) / len(recent)
        if spread < 0.05 and avg < 0.80:
            alerts.append(
                {
                    "type": "quality_plateau",
                    "message": (
                        f"Quality plateaued at {avg:.2f} "
                        f"(spread {spread:.3f} over last 5 iterations)"
                    ),
                }
            )

    # Low quality warning: any recent score below 0.50
    recent_scores = (
        quality_history[-3:] if len(quality_history) >= 3 else quality_history
    )
    for i, score in enumerate(recent_scores):
        if score < 0.50:
            alerts.append(
                {
                    "type": "low_quality",
                    "message": f"Quality score {score:.2f} is below minimum threshold (0.50)",
                }
            )
            break  # One alert is enough

    return alerts


def format_quality_display(score: float, letter_grade: str) -> str:
    """Produce dashboard display string.

    Example: "B (0.78)"

    Args:
        score: quality score (0.0-1.0).
        letter_grade: letter grade string.

    Returns:
        formatted display string.
    """
    return f"{letter_grade} ({score:.2f})"


def compute_effective_weights(
    category_scores: dict[str, Optional[float]],
) -> dict[str, float]:
    """Compute effective weights after N/A redistribution.

    Args:
        category_scores: mapping of category name to score or None.

    Returns:
        mapping of active category names to their effective weights.
    """
    active_weight_sum = sum(
        CATEGORIES[name][0]
        for name, score in category_scores.items()
        if score is not None
    )
    if active_weight_sum == 0:
        return {}

    return {
        name: CATEGORIES[name][0] / active_weight_sum
        for name, score in category_scores.items()
        if score is not None
    }
