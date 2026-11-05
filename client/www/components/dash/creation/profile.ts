export type ProfileMeta = {
  heard: string;
  experience: 'vibe-dev' | 'junior-dev' | 'senior-dev';
};
export function profileMeta(heard: string, experience: string): ProfileMeta {
  if (!heard.trim())
    throw new Error('Tell us how you heard about BackendRelay.');
  if (!['vibe-dev', 'junior-dev', 'senior-dev'].includes(experience))
    throw new Error('Choose your coding experience.');
  return {
    heard: heard.trim(),
    experience: experience as ProfileMeta['experience'],
  };
}
