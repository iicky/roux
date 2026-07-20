"""Data models exercising classes, inheritance, decorators, type hints."""

from dataclasses import dataclass
from typing import Optional


@dataclass
class User:
    """A regular user."""

    id: int
    name: str
    email: Optional[str] = None

    def display_name(self) -> str:
        """Return a friendly display name with email if present."""
        if self.email:
            return f"{self.name} <{self.email}>"
        return self.name


class AdminUser(User):
    """An administrator with elevated privileges."""

    def __init__(
        self,
        id: int,
        name: str,
        email: Optional[str] = None,
        level: int = 1,
    ) -> None:
        super().__init__(id=id, name=name, email=email)
        self.level = level

    def can_edit(self, resource: str) -> bool:
        """Return whether this admin can edit the given resource."""
        return self.level >= 2 or resource.startswith("public/")
