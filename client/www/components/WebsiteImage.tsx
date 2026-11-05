import { forwardRef, type ImgHTMLAttributes } from 'react';

type StaticImage = { src: string; width?: number; height?: number };

type WebsiteImageProps = Omit<
  ImgHTMLAttributes<HTMLImageElement>,
  'src' | 'width' | 'height' | 'loading'
> & {
  src: string | StaticImage;
  width?: number;
  height?: number;
  fill?: boolean;
  priority?: boolean;
  loading?: 'eager' | 'lazy';
};

const WebsiteImage = forwardRef<HTMLImageElement, WebsiteImageProps>(
  function WebsiteImage(
    { src, width, height, fill, priority, loading, style, ...props },
    ref,
  ) {
    const image = typeof src === 'string' ? { src } : src;
    return (
      <img
        {...props}
        ref={ref}
        src={image.src}
        width={width ?? image.width}
        height={height ?? image.height}
        loading={priority ? 'eager' : loading}
        {...(priority ? { fetchpriority: 'high' } : {})}
        style={
          fill
            ? {
                position: 'absolute',
                width: '100%',
                height: '100%',
                inset: 0,
                ...style,
              }
            : style
        }
      />
    );
  },
);

export default WebsiteImage;
