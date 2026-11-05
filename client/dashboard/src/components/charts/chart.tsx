import { Activity } from 'lucide-react';
import { useState } from 'react';
import {
  Bar,
  BarChart,
  CartesianGrid,
  Line,
  LineChart,
  ResponsiveContainer,
  Tooltip as RechartsTooltip,
  XAxis,
  YAxis,
} from 'recharts';
import { EmptyState } from '@/components/feedback/empty-state';
import { SectionCard } from '@/components/layout/section-card';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { formatCount } from '@/lib/format';

export interface ChartSeries {
  key: string;
  label: string;
  /** Defaults to the next `--chart-n` token. */
  color?: string;
}

export interface ChartRange {
  value: string;
  label: string;
}

export type ChartRow = Record<string, string | number | null>;

export interface ChartProps {
  title: string;
  description?: string;
  data: ReadonlyArray<ChartRow>;
  /** Field used for the X axis. */
  xKey: string;
  series: ReadonlyArray<ChartSeries>;
  variant?: 'line' | 'bar';
  /** Optional range picker, for example 24h / 7d / 30d. */
  ranges?: ReadonlyArray<ChartRange>;
  range?: string;
  onRangeChange?: (range: string) => void;
  emptyMessage?: string;
  height?: number;
  className?: string;
}

function seriesColor(series: ChartSeries, index: number): string {
  return series.color ?? `var(--chart-${String((index % 5) + 1)})`;
}

const tooltipStyle = {
  backgroundColor: 'var(--card)',
  border: '1px solid var(--border)',
  borderRadius: '0.5rem',
  fontSize: '12px',
} as const;

function formatCell(value: string | number | null): string {
  if (value === null) {
    return '—';
  }
  return typeof value === 'number' ? formatCount(value) : value;
}

/**
 * Recharts wrapper with a title, a range select, a "Show as table" toggle and
 * an empty state (B.4). The table view is the accessible alternative to the
 * SVG chart.
 */
export function Chart({
  title,
  description,
  data,
  xKey,
  series,
  variant = 'line',
  ranges,
  range,
  onRangeChange,
  emptyMessage = 'There is nothing to chart for this range.',
  height = 240,
  className,
}: ChartProps) {
  const [showTable, setShowTable] = useState(false);
  const chartData = [...data];

  const actions = (
    <>
      {ranges && ranges.length > 0 && onRangeChange ? (
        <Select
          value={range ?? ranges[0]?.value ?? ''}
          onValueChange={(value) => {
            if (typeof value === 'string') {
              onRangeChange(value);
            }
          }}
        >
          <SelectTrigger aria-label="Chart range" className="h-7 w-28">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {ranges.map((entry) => (
              <SelectItem key={entry.value} value={entry.value}>
                {entry.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      ) : null}
      <Button
        type="button"
        variant="ghost"
        size="sm"
        aria-pressed={showTable}
        onClick={() => {
          setShowTable((current) => !current);
        }}
      >
        {showTable ? 'Show as chart' : 'Show as table'}
      </Button>
    </>
  );

  return (
    <SectionCard
      title={title}
      description={description}
      actions={actions}
      className={className}
    >
      {data.length === 0 ? (
        <EmptyState
          icon={Activity}
          title="No data yet"
          description={emptyMessage}
        />
      ) : showTable ? (
        <Table>
          <TableCaption className="sr-only">{title}</TableCaption>
          <TableHeader>
            <TableRow>
              <TableHead scope="col">{xKey}</TableHead>
              {series.map((entry) => (
                <TableHead key={entry.key} scope="col" className="text-right">
                  {entry.label}
                </TableHead>
              ))}
            </TableRow>
          </TableHeader>
          <TableBody>
            {data.map((row, rowIndex) => (
              <TableRow key={String(row[xKey] ?? rowIndex)}>
                <TableCell>{formatCell(row[xKey] ?? null)}</TableCell>
                {series.map((entry) => (
                  <TableCell
                    key={entry.key}
                    className="text-right tabular-nums"
                  >
                    {formatCell(row[entry.key] ?? null)}
                  </TableCell>
                ))}
              </TableRow>
            ))}
          </TableBody>
        </Table>
      ) : (
        <div className="w-full" style={{ height }}>
          <ResponsiveContainer width="100%" height="100%">
            {variant === 'bar' ? (
              <BarChart data={chartData} accessibilityLayer>
                <CartesianGrid
                  stroke="var(--border)"
                  strokeDasharray="2 4"
                  vertical={false}
                />
                <XAxis
                  dataKey={xKey}
                  tickLine={false}
                  axisLine={{ stroke: 'var(--border)' }}
                  tick={{ fill: 'var(--muted-foreground)', fontSize: 12 }}
                />
                <YAxis
                  width={48}
                  tickLine={false}
                  axisLine={false}
                  tick={{ fill: 'var(--muted-foreground)', fontSize: 12 }}
                />
                <RechartsTooltip
                  contentStyle={tooltipStyle}
                  labelStyle={{ color: 'var(--foreground)' }}
                  itemStyle={{ color: 'var(--foreground)' }}
                  cursor={{ fill: 'var(--muted)' }}
                />
                {series.map((entry, index) => (
                  <Bar
                    key={entry.key}
                    dataKey={entry.key}
                    name={entry.label}
                    fill={seriesColor(entry, index)}
                    radius={[2, 2, 0, 0]}
                  />
                ))}
              </BarChart>
            ) : (
              <LineChart data={chartData} accessibilityLayer>
                <CartesianGrid
                  stroke="var(--border)"
                  strokeDasharray="2 4"
                  vertical={false}
                />
                <XAxis
                  dataKey={xKey}
                  tickLine={false}
                  axisLine={{ stroke: 'var(--border)' }}
                  tick={{ fill: 'var(--muted-foreground)', fontSize: 12 }}
                />
                <YAxis
                  width={48}
                  tickLine={false}
                  axisLine={false}
                  tick={{ fill: 'var(--muted-foreground)', fontSize: 12 }}
                />
                <RechartsTooltip
                  contentStyle={tooltipStyle}
                  labelStyle={{ color: 'var(--foreground)' }}
                  itemStyle={{ color: 'var(--foreground)' }}
                  cursor={{ stroke: 'var(--border)' }}
                />
                {series.map((entry, index) => (
                  <Line
                    key={entry.key}
                    type="monotone"
                    dataKey={entry.key}
                    name={entry.label}
                    stroke={seriesColor(entry, index)}
                    strokeWidth={2}
                    dot={false}
                  />
                ))}
              </LineChart>
            )}
          </ResponsiveContainer>
        </div>
      )}
    </SectionCard>
  );
}
