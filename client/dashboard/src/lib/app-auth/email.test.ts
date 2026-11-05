import { describe, expect, it } from 'vitest';
import type { SenderVerification } from '@/lib/api/endpoints';
import {
  DEFAULT_MAGIC_CODE_BODY,
  DEFAULT_MAGIC_CODE_SUBJECT,
  effectiveSenderName,
  emailTemplateDirty,
  emailTemplateDraft,
  emailTemplateProblem,
  expiryLabel,
  expiryMinutesProblem,
  senderStatus,
  substituteSampleVars,
} from './email';

const template = {
  id: 'template-1',
  name: 'todo-app',
  email: 'noreply@example.com',
  subject: 'Use {code} to sign in',
  body: 'Hello {app_title}, {code} for {user_email} expires in {expiration}.',
};

const verification = (
  value: Partial<SenderVerification> & {
    instant?: { 'verified?'?: boolean | null };
  },
): SenderVerification => ({
  instant: { 'verified?': false },
  verification: null,
  domain: null,
  ...value,
});

describe('email template drafts', () => {
  it('seeds from the stored template or the defaults', () => {
    expect(emailTemplateDraft(undefined)).toEqual({
      senderName: '',
      senderEmail: '',
      subject: DEFAULT_MAGIC_CODE_SUBJECT,
      body: DEFAULT_MAGIC_CODE_BODY,
    });
    expect(emailTemplateDraft(template)).toEqual({
      senderName: 'todo-app',
      senderEmail: 'noreply@example.com',
      subject: 'Use {code} to sign in',
      body: template.body,
    });
  });

  it('treats an empty sender name as the app title', () => {
    expect(effectiveSenderName('', 'todo-app')).toBe('todo-app');
    expect(effectiveSenderName('   ', 'todo-app')).toBe('todo-app');
    expect(effectiveSenderName('Support', 'todo-app')).toBe('Support');
  });

  it('compares the draft with the saved template', () => {
    const draft = emailTemplateDraft(template);
    expect(emailTemplateDirty(draft, template, 'todo-app')).toBe(false);
    expect(
      emailTemplateDirty({ ...draft, senderName: '' }, template, 'todo-app'),
    ).toBe(false);
    expect(
      emailTemplateDirty(
        { ...draft, senderEmail: 'NOREPLY@example.com' },
        template,
        'todo-app',
      ),
    ).toBe(false);
    expect(
      emailTemplateDirty(
        { ...draft, subject: 'New {code}' },
        template,
        'todo-app',
      ),
    ).toBe(true);
    expect(
      emailTemplateDirty(
        { ...draft, senderEmail: 'hello@example.com' },
        template,
        'todo-app',
      ),
    ).toBe(true);
  });

  it('requires the code variable in the subject and body', () => {
    expect(emailTemplateProblem(emailTemplateDraft(template))).toBeUndefined();
    expect(
      emailTemplateProblem({
        ...emailTemplateDraft(template),
        subject: 'No variable',
      }),
    ).toBe('The subject must include {code}.');
    expect(
      emailTemplateProblem({
        ...emailTemplateDraft(template),
        body: '',
      }),
    ).toBe('Enter an email body.');
  });
});

describe('sample rendering', () => {
  it('substitutes every variable and can escape HTML', () => {
    expect(
      substituteSampleVars(template.body, {
        appTitle: 'todo-app',
        expiryMinutes: 15,
      }),
    ).toBe(
      'Hello todo-app, 123456 for happyuser@gmail.com expires in 15 minutes.',
    );
    expect(
      substituteSampleVars('{app_title}', {
        appTitle: '<b>todo</b>',
        expiryMinutes: 60,
        escape: true,
      }),
    ).toBe('&lt;b&gt;todo&lt;/b&gt;');
  });

  it('labels the expiry the way the server does', () => {
    expect(expiryLabel(undefined)).toBe('15 minutes');
    expect(expiryLabel(1)).toBe('1 minute');
    expect(expiryLabel(60)).toBe('1 hour');
  });

  it('accepts only whole minutes between 1 and 60', () => {
    expect(expiryMinutesProblem('30')).toBeUndefined();
    expect(expiryMinutesProblem('0')).toBe('Choose between 1 and 60 minutes.');
    expect(expiryMinutesProblem('61')).toBe('Choose between 1 and 60 minutes.');
    expect(expiryMinutesProblem('3.5')).toBe(
      'Enter a whole number of minutes.',
    );
  });
});

describe('sender status', () => {
  it('reports the default when there is no custom sender', () => {
    expect(senderStatus('', verification({}))).toEqual({
      kind: 'default',
      label: 'Not set',
    });
  });

  it('stays pending until the emailed code is confirmed', () => {
    expect(senderStatus('noreply@example.com', verification({}))).toEqual({
      kind: 'pending',
      label: 'Pending',
      detail: undefined,
    });
  });

  it('accepts a confirmed Postmark sender', () => {
    expect(
      senderStatus(
        'noreply@example.com',
        verification({
          instant: { 'verified?': true },
          verification: { Confirmed: true },
        }),
      ),
    ).toEqual({ kind: 'verified', label: 'Verified' });
  });

  it('accepts a Cloudflare sender with an enabled domain', () => {
    expect(
      senderStatus(
        'noreply@example.com',
        verification({
          instant: { 'verified?': true },
          domain: { cloudflare_enabled: true },
        }),
      ),
    ).toEqual({ kind: 'verified', label: 'Verified' });
  });

  it('flags a domain that is no longer enabled', () => {
    expect(
      senderStatus(
        'noreply@example.com',
        verification({
          instant: { 'verified?': true },
          domain: {
            cloudflare_enabled: false,
            last_error: 'Email Sending is not enabled.',
          },
        }),
      ),
    ).toEqual({
      kind: 'domain-disabled',
      label: 'Domain no longer enabled — using the default sender',
      detail: 'Email Sending is not enabled.',
    });
  });
});
