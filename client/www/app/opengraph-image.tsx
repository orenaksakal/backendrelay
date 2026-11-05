import { generateOgImage } from '@/lib/ogImage';

export const alt = 'BackendRelay: The best backend for AI-coded apps';
export const size = { width: 1200, height: 630 };
export const contentType = 'image/png';

export default async function Image() {
  return generateOgImage({
    brand: 'BackendRelay',
    title: 'The best backend for AI-coded apps',
  });
}
