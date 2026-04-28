"""String enums for BOI status values. Subclass str for SQLite/string compatibility."""
from enum import Enum


class SpecStatus(str, Enum):
    QUEUED = "queued"
    RUNNING = "running"
    REQUEUED = "requeued"
    ASSIGNING = "assigning"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELED = "canceled"
    NEEDS_REVIEW = "needs_review"


class Phase(str, Enum):
    EXECUTE = "execute"
    DECOMPOSE = "decompose"
    EVALUATE = "evaluate"
    CRITIC = "task-verify"
    REVIEW = "review"


class TaskStatus(str, Enum):
    PENDING = "PENDING"
    DONE = "DONE"
    FAILED = "FAILED"
    IN_PROGRESS = "IN_PROGRESS"
    EXPERIMENT_PROPOSED = "EXPERIMENT_PROPOSED"
