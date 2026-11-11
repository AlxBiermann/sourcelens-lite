export interface User {
  id: string;
  name: string;
  email: string;
}

const users = new Map<string, User>();

export function getUserById(id: string): User | undefined {
  // Look the user up in the in-memory store.
  const user = users.get(id);
  if (!user) {
    return undefined;
  }
  return user;
}

export class UserService {
  private readonly cache = new Map<string, User>();

  constructor(private readonly prefix: string) {}

  findByEmail(email: string): User | undefined {
    for (const user of users.values()) {
      if (user.email === email) {
        return user;
      }
    }
    return undefined;
  }

  rename(id: string, name: string): void {
    const user = getUserById(id);
    if (user) {
      user.name = name;
    }
  }
}
