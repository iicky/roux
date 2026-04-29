"""Tiny example package used as a Python extractor fixture."""

from .models import AdminUser, User
from .handlers import handle_request

__all__ = ["AdminUser", "User", "handle_request"]
