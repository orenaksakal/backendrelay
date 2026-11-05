import { ImageResponse } from '@vercel/og';
import { readFileSync } from 'fs';
import { join } from 'path';

const SIZE = { width: 1200, height: 630 };

function readFont(filename: string): Buffer {
  return readFileSync(join(process.cwd(), 'public', 'fonts', filename));
}

export async function loadFonts() {
  const swRegular = readFont('Switzer-Regular.woff');
  const swSemibold = readFont('Switzer-Semibold.woff');
  return [
    {
      name: 'Switzer',
      data: swRegular,
      style: 'normal' as const,
      weight: 400 as const,
    },
    {
      name: 'Switzer',
      data: swSemibold,
      style: 'normal' as const,
      weight: 600 as const,
    },
  ];
}

export function Logo({ size = 64 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 32 32"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
    >
      <rect width="32" height="32" rx="8" fill="#171717" />
      <path d="M8 10h11l5 6-5 6H8l5-6Zm5 6h11" stroke="white" strokeWidth="2" />
    </svg>
  );
}

export async function generateOgImage({
  title,
  section,
  brand = 'BackendRelay',
  layout = 'default',
}: {
  title?: React.ReactNode;
  section?: string;
  brand?: string;
  layout?: 'default' | 'centered';
}) {
  const fonts = await loadFonts();

  const content =
    layout === 'centered' ? (
      <div
        style={{
          background: '#fff',
          height: '100%',
          width: '100%',
          display: 'flex',
          flexDirection: 'column',
          padding: '60px 80px',
        }}
      >
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
          }}
        >
          <Logo size={40} />
          <div
            style={{
              fontSize: 36,
              fontFamily: 'Switzer',
              marginLeft: 14,
              fontWeight: 700,
              color: '#000',
            }}
          >
            {brand}
          </div>
        </div>
        {title ? (
          <div
            style={{
              display: 'flex',
              flex: 1,
              alignItems: 'center',
              justifyContent: 'center',
            }}
          >
            <div
              style={{
                fontSize: 56,
                fontFamily: 'Switzer',
                fontWeight: 600,
                lineHeight: 1.3,
                color: '#000',
                textAlign: 'center',
              }}
            >
              {title}
            </div>
          </div>
        ) : null}
      </div>
    ) : (
      <div
        style={{
          background: '#000',
          height: '100%',
          width: '100%',
          display: 'flex',
          alignItems: 'flex-start',
          justifyContent: 'flex-end',
          flexDirection: 'column',
          padding: '60px 80px',
        }}
      >
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
          }}
        >
          <Logo />
          <div
            style={{
              fontSize: 64,
              fontFamily: 'Switzer',
              marginLeft: 20,
              fontWeight: 700,
              color: '#fff',
            }}
          >
            {brand}
          </div>
          {section ? (
            <div
              style={{
                fontSize: 64,
                fontFamily: 'Switzer',
                marginLeft: 16,
                color: '#888',
              }}
            >
              {section}
            </div>
          ) : null}
        </div>
        {title ? (
          <div
            style={{
              display: 'flex',
              fontSize: 40,
              fontFamily: 'Switzer',
              fontWeight: 400,
              marginTop: 24,
              lineHeight: 1.3,
              color: '#aaa',
              maxWidth: 1040,
            }}
          >
            {title}
          </div>
        ) : null}
      </div>
    );

  return new ImageResponse(content, {
    ...SIZE,
    fonts,
  });
}
