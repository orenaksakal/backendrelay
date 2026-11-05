/**
 * The mocked email sign-in flow. `POST /dash/auth/verify_magic_code` accepts
 * this code for any email and answers with the session below.
 */
export const mockSignInCode = '123456';

export const mockSignInToken = 'mock-session-token';

export const mockSignInUser = {
  id: 'user-1',
  email: 'owner@example.com',
};
