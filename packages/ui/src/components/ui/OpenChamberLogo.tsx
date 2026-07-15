import React from 'react';
import { useI18n } from '@/lib/i18n';

interface OpenChamberLogoProps {
  className?: string;
  width?: number;
  height?: number;
  isAnimated?: boolean;
}

/**
 * OpenChamber logo — dark-tech isometric 3D core with orbital rings and lightning bolt.
 * Design originally provided at 600×600, scaled down to 100×100 viewBox.
 */
export const OpenChamberLogo: React.FC<OpenChamberLogoProps> = ({
  className = '',
  width = 70,
  height = 70,
  isAnimated = false,
}) => {
  const { t } = useI18n();
  const uid = React.useId();

  return (
    <svg
      width={width}
      height={height}
      viewBox="0 0 100 100"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      className={className}
      role="img"
      aria-label={t('openChamberLogo.aria.logo')}
    >
      {isAnimated ? (
        <style>{`@keyframes oc-glow-${uid}{0%,100%{opacity:1}50%{opacity:0.6}}.oc-glow{animation:oc-glow-${uid} 1.8s ease-in-out infinite}@media (prefers-reduced-motion:reduce){.oc-glow{animation:none}}`}</style>
      ) : null}
      <defs>
        <linearGradient id={`${uid}-coreTop`} x1="0%" y1="0%" x2="100%" y2="100%">
          <stop offset="0%" stopColor="#7bf2ff" />
          <stop offset="100%" stopColor="#29cce8" />
        </linearGradient>
        <linearGradient id={`${uid}-coreLeft`} x1="0%" y1="0%" x2="100%" y2="100%">
          <stop offset="0%" stopColor="#0f8ba4" />
          <stop offset="100%" stopColor="#064d5f" />
        </linearGradient>
        <linearGradient id={`${uid}-coreRight`} x1="0%" y1="0%" x2="100%" y2="100%">
          <stop offset="0%" stopColor="#14aac7" />
          <stop offset="100%" stopColor="#08748a" />
        </linearGradient>
        <linearGradient id={`${uid}-ring`} x1="0%" y1="0%" x2="100%" y2="100%">
          <stop offset="0%" stopColor="#bfe6ff" stopOpacity="0.9" />
          <stop offset="50%" stopColor="#33e0ff" stopOpacity="0.7" />
          <stop offset="100%" stopColor="#006685" stopOpacity="0.9" />
        </linearGradient>
        <filter id={`${uid}-shadow`} x="-30%" y="-30%" width="160%" height="160%">
          <feDropShadow dx="0" dy="3" stdDeviation="3" floodColor="#000000" floodOpacity="0.8" />
          <feDropShadow dx="0" dy="1" stdDeviation="1" floodColor="#1bb9d4" floodOpacity="0.2" />
        </filter>
        <filter id={`${uid}-glow`} x="-50%" y="-50%" width="200%" height="200%">
          <feGaussianBlur stdDeviation="2" result="blur" />
          <feMerge>
            <feMergeNode in="blur" />
            <feMergeNode in="SourceGraphic" />
          </feMerge>
        </filter>
        <filter id={`${uid}-glowStrong`} x="-50%" y="-50%" width="200%" height="200%">
          <feGaussianBlur stdDeviation="3" result="blur" />
          <feMerge>
            <feMergeNode in="blur" />
            <feMergeNode in="blur" />
            <feMergeNode in="SourceGraphic" />
          </feMerge>
        </filter>
      </defs>

      {/* Grid lines */}
      <g fill="none" stroke="#00d5ff" strokeOpacity="0.15" strokeWidth="0.5">
        <circle cx="50" cy="50" r="40" strokeDasharray="0.7 1.3" />
        <circle cx="50" cy="50" r="37" strokeDasharray="2 2" />
        <path d="M 50 10 L 50 90 M 10 50 L 90 50" strokeWidth="0.8" strokeDasharray="0.2 1.7" />
        <path d="M 20 20 L 80 80" strokeDasharray="3 7" />
        <path d="M 80 20 L 20 80" strokeDasharray="3 7" />
      </g>

      {/* 3D Isometric Core */}
      <g filter={`url(#${uid}-shadow)`}>
        {/* Base platform */}
        <polygon points="50,30 70,40 50,50 30,40" fill="#042b36" />
        <polygon points="30,40 50,50 50,70 30,60" fill="#021e25" />
        <polygon points="50,50 70,40 70,60 50,70" fill="#032630" />

        {/* Top face */}
        <polygon points="50,35 64,42 50,49 36,42" fill={`url(#${uid}-coreTop)`} stroke="#a1f5ff" strokeWidth="0.5" strokeOpacity="0.5" />
        {/* Left face */}
        <polygon points="36,42 50,49 50,62 36,55" fill={`url(#${uid}-coreLeft)`} />
        {/* Right face */}
        <polygon points="50,49 64,42 64,55 50,62" fill={`url(#${uid}-coreRight)`} />

        {/* Surface grooves */}
        <polygon points="50,38 57,41 50,44 43,41" fill="#0d5869" fillOpacity="0.6" stroke="#00c3ff" strokeWidth="0.3" />
        <polygon points="43,41 50,44 50,50 43,47" fill="#07323e" fillOpacity="0.8" />
        <polygon points="50,44 57,41 57,47 50,50" fill="#0a4856" fillOpacity="0.8" />
      </g>

      {/* Orbital Rings */}
      <g filter={`url(#${uid}-shadow)`}>
        <ellipse cx="50" cy="50" rx="25" ry="9" fill="none" stroke={`url(#${uid}-ring)`} strokeWidth="1.5" strokeDasharray="4 2 1 2" transform="rotate(-30 50 50)" />
        <ellipse cx="50" cy="50" rx="25" ry="9" fill="none" stroke="#FFFFFF" strokeOpacity="0.8" strokeWidth="0.5" strokeDasharray="3 3" transform="rotate(-30 50 50)" />
        <ellipse cx="50" cy="50" rx="28" ry="10" fill="none" stroke={`url(#${uid}-ring)`} strokeWidth="1" strokeDasharray="2 2" transform="rotate(40 50 50)" />

        {/* Node dots */}
        <g filter={`url(#${uid}-glow)`}>
          <circle cx="50" cy="28" r="1" fill="#d8fcff" />
          <circle cx="72" cy="43" r="1" fill="#19d4ff" />
          <circle cx="47" cy="75" r="1.2" fill="#ffffff" />
          <circle cx="28" cy="57" r="1" fill="#1ad4ff" />
        </g>
      </g>

      {/* Lightning Bolt */}
      {isAnimated ? (
        <g className="oc-glow">
          <g filter={`url(#${uid}-glowStrong)`}>
            <polygon points="50,33 54,45 52,45 53,58 48,51 50,51 49,33" fill="none" stroke="#ffffff" strokeWidth="0.8" />
          </g>
          <g filter={`url(#${uid}-glow)`}>
            <polygon points="50,33 54,45 52,45 53,58 48,51 50,51 49,33" fill="#ffffff" />
            <polygon points="50,35 52,44 51,44 52,53 48,49 50,49 50,35" fill="#bbf9ff" />
          </g>
        </g>
      ) : (
        <>
          <g filter={`url(#${uid}-glowStrong)`}>
            <polygon points="50,33 54,45 52,45 53,58 48,51 50,51 49,33" fill="none" stroke="#ffffff" strokeWidth="0.8" />
          </g>
          <g filter={`url(#${uid}-glow)`}>
            <polygon points="50,33 54,45 52,45 53,58 48,51 50,51 49,33" fill="#ffffff" />
            <polygon points="50,35 52,44 51,44 52,53 48,49 50,49 50,35" fill="#bbf9ff" />
          </g>
        </>
      )}

      {/* Text */}
      <g fill="#57828f" fontFamily="'Montserrat','Segoe UI',sans-serif" fontWeight="900" letterSpacing="1.7">
        <text x="50" y="88" fontSize="3" textAnchor="middle" fill="#1cc4e0" fillOpacity="0.7">AUTOMATED GRID</text>
        <text x="50" y="93" fontSize="1.7" textAnchor="middle" fill="#ffffff" fillOpacity="0.2" letterSpacing="0.7">HUMANLESS OPERATIONS</text>
      </g>
    </svg>
  );
};
