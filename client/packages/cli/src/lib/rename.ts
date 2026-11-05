import boxen from 'boxen';
import { Effect } from 'effect';
import { UI } from '../ui/index.ts';
import { ResolveRenamePrompt } from '../util/renamePrompt.ts';
import { runUIEffect } from './ui.ts';

export const resolveRenames = <T extends string>(
  created: T,
  promptData: ({ from: T; to: T } | T)[],
  extraInfo?: unknown,
) =>
  runUIEffect(
    new ResolveRenamePrompt(
      created,
      promptData,
      extraInfo,
      UI.modifiers.piped([
        (out) =>
          boxen(out, {
            dimBorder: true,
            padding: {
              left: 1,
              right: 1,
            },
          }),
        UI.modifiers.vanishOnComplete,
      ]),
    ),
  );

export const resolveRenamesPromise = <T extends string>(
  created: T,
  promptData: ({ from: T; to: T } | T)[],
  extraInfo?: unknown,
) => Effect.runPromise(resolveRenames(created, promptData, extraInfo));
