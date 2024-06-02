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

test("adds new options", async ({ page }) => {
  await page.goto("/");

  const button = page.getByText("ADD OPTION");

  expect((await page.getByPlaceholder("a choice").all()).length).toBe(2);
  await button.click();
  expect((await page.getByPlaceholder("a choice").all()).length).toBe(3);
  await button.click();
  expect((await page.getByPlaceholder("a choice").all()).length).toBe(4);
});
