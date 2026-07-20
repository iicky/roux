/** Core types for the tiny TypeScript fixture. */

export interface User {
  id: number;
  name: string;
  email?: string;
}

export type UserRole = "admin" | "user" | "guest";

/** Generic store keyed by numeric id. */
export class UserStore<T extends User> {
  private users: Map<number, T> = new Map();

  add(user: T): void {
    this.users.set(user.id, user);
  }

  get(id: number): T | undefined {
    return this.users.get(id);
  }
}

export interface AdminUser extends User {
  level: number;
}
