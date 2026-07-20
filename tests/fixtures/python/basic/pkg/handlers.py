"""Request dispatch — exercises cross-module imports and isinstance checks."""

from .models import AdminUser, User


def handle_request(user: User, action: str) -> str:
    """Dispatch based on user type."""
    if isinstance(user, AdminUser):
        return _admin_handler(user, action)
    return _user_handler(user, action)


def _user_handler(user: User, action: str) -> str:
    return f"user {user.display_name()} did {action}"


def _admin_handler(user: AdminUser, action: str) -> str:
    if user.can_edit(action):
        return f"admin {user.display_name()} edited {action}"
    return f"denied: {user.display_name()} cannot edit {action}"
