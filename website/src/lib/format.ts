const idr = new Intl.NumberFormat('id-ID', {
  style: 'currency',
  currency: 'IDR',
  maximumFractionDigits: 0,
});

export function formatIdr(value: number): string {
  return idr.format(value);
}

export function formatCount(value: number): string {
  return new Intl.NumberFormat('en-US').format(value);
}
