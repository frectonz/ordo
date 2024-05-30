import { test, expect } from "@playwright/test";

test("has a form", async ({ page }) => {
  await page.goto("/");

  const form = page.getByTestId("create-room-form");

  await expect(form).toBeVisible();
  await expect(form).toHaveAttribute("hx-post", "/rooms");
  await expect(form).toHaveAttribute("hx-ext", "json-enc");
  await expect(form).toHaveAttribute("hx-target", "main");
  await expect(form).toHaveAttribute("hx-swap", "innerHTML");
});
