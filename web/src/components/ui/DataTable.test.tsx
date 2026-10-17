import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { Link, MemoryRouter, useLocation, useNavigate } from 'react-router-dom';
import { DataTable, Row } from './DataTable';

function Location() {
  const location = useLocation();
  const navigate = useNavigate();
  return (
    <>
      <output aria-label="Current page">{location.pathname}</output>
      <button onClick={() => void navigate(-1)}>Back</button>
    </>
  );
}

function renderRow() {
  render(
    <MemoryRouter initialEntries={['/wallet']}>
      <Location />
      <DataTable columns={[{ key: 'item', header: 'Item' }]}>
        <Row to="/explorer/tx/example">
          <td>
            <Link to="/explorer/tx/example">Example transaction</Link>
          </td>
        </Row>
      </DataTable>
    </MemoryRouter>,
  );
}

describe('clickable table rows', () => {
  it('still opens the destination when the row outside its link is clicked', async () => {
    renderRow();
    await userEvent.click(screen.getByRole('row', { name: 'Example transaction' }));
    expect(screen.getByRole('status', { name: 'Current page' })).toHaveTextContent(
      '/explorer/tx/example',
    );
  });

  it('navigates once when its link is clicked, so one Back returns to the prior page', async () => {
    renderRow();
    await userEvent.click(screen.getByRole('link', { name: 'Example transaction' }));
    expect(screen.getByRole('status', { name: 'Current page' })).toHaveTextContent(
      '/explorer/tx/example',
    );
    await userEvent.click(screen.getByRole('button', { name: 'Back' }));
    expect(screen.getByRole('status', { name: 'Current page' })).toHaveTextContent('/wallet');
  });

  it('leaves the current tab in place for a modified link click', async () => {
    renderRow();
    const user = userEvent.setup();
    await user.keyboard('{Meta>}');
    await user.click(screen.getByRole('link', { name: 'Example transaction' }));
    await user.keyboard('{/Meta}');
    expect(screen.getByRole('status', { name: 'Current page' })).toHaveTextContent('/wallet');
  });
});
