/** Async request handlers. */

import { AdminUser, User, UserStore } from "./types";

export async function fetchUser(id: number): Promise<User | null> {
  return Promise.resolve(null);
}

export class RequestHandler {
  constructor(private readonly store: UserStore<User>) {}

  async handle(action: string, userId: number): Promise<string> {
    const user = this.store.get(userId);
    if (!user) {
      return "unknown user";
    }
    return `${user.name} did ${action}`;
  }
}

export function isAdmin(user: User): user is AdminUser {
  return (user as AdminUser).level !== undefined;
}
